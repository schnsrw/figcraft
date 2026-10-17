# SaaS implementation tracker

> **Status:** S1 complete (22/22); S2 complete (21/21); S3 complete (20/20); Core slice complete (5/5); S4 complete (25/25); S5 in progress (20/25); S6 in progress (5/31) · **Last updated:** 2026-10-18

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
| S3 Object storage and migration | complete | 20 / 20 | Verified object storage and repeatable migration |
| **Core slice** Browser co-editing on the cloud stack (pulled forward) | complete | 5 / 5 | Two browsers co-edit through the gateway and survive a cloud restart |
| S4 Identity, tenancy and authorization | complete | 25 / 25 | Isolation and role suite passes |
| S5 Distributed collaboration relay | in progress | 20 / 25 | Fenced multi-replica relay passes failure tests |
| S6 Browser, desktop and embedded integration | in progress | 5 / 31 | All clients complete cloud workflows |
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
  Follow-up (2026-10-17): event errors distinguish `Unavailable` (connection loss, pool timeout),
  `Timeout` (57014/55P03) and `SerializationFailure` (40001/40P01) from `Database`, with retry
  safety documented; a pending idempotency key past its lease with a matching request is `TakenOver`
  for re-execution and an expired key counts as absent, with the race probes unchanged.
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
  Follow-up (2026-10-17): holds record who placed them and why in `retention_holds` (same
  transaction as the reference); re-placing keeps the original placer, reusing a hold id elsewhere
  is `Conflict`, and `list_retention_holds` pages placer and reason.

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
  Follow-up (2026-10-17): `quarantine` marks a document whose history cannot be restored (`log_gap`,
  `log_corrupt`, `snapshot_chain_unverifiable`, `replay_failed`, `operator`);
  `DurableRoom::open`/`recover` refuse quarantined documents before claiming and quarantine on a
  failed restore, and clearing is audited. Snapshot and library writes now register and commit the
  pending object row before uploading, so a rolled-back caller leaves a row GC can settle instead of
  untracked bytes.
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
  Follow-up (2026-10-17): plugin lookup-or-create uses `INSERT ... ON CONFLICT` on the partial
  unique index `plugins_live_publisher_name_idx` instead of an advisory lock; a
  concurrent-first-install probe ends with one live row.
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
  Follow-up (2026-10-17): an orphan-key sweep deletes bucket keys with no `objects` row past the
  minimum age (all versions, dry run supported, live reservations' staging keys untouched), and
  stuck pending rows are settled through read-back verification into collectable rejected rows.
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
- [x] **S3-018** Implement repeatable filesystem-to-PostgreSQL/S3 migration with per-resource
  checkpoints.
  Evidence: `legacy_import::import_plan` and `import_document`
  (crates/cloud-storage/src/legacy_import.rs) run a `MigrationPlan` against a stopped legacy data
  directory. A plan is refused if its format is unknown, its bucket differs from the store's, or it
  has any blocker other than `exclusion_approval_required`, which
  `ImportOptions::approve_exclusions` accepts. Excluded documents are never touched and come back as
  `skipped`; one failing document does not stop the others. Each document's snapshot.bin and log.bin
  are re-read read-only and bounded by `ScanLimits` (links refused, sizes capped). The snapshot must
  still decode at the planned seq and size, and the log's replayable accepted batches (same
  generation, after the snapshot seq, consecutive, stop at the first damaged record) must still
  equal `events.count` and end at `document.current_seq`. Otherwise the result is `LegacyChanged`.
  Four idempotent stages follow, each detected from existing rows. (1) Document: created with the
  plan's derived id at `current_seq` = snapshot seq; an existing row is resumed, and another
  workspace/project or a non-active row is `Diverged`. (2) The `figcraft-server` external mapping.
  (3) Snapshot: the SessionState body is uploaded content-addressed under `keys::snapshot`, then
  register_object + verify_object, register_snapshot + verify_snapshot, a `document_snapshot` object
  reference and set_current_snapshot. An existing object with another digest is `LegacyChanged`, and
  a rejected one is `SnapshotRejected`. (4) Events: one claim_sequencer, then append_events in
  chunks of at most 256 batches or 8 MiB of legacy record bytes per transaction, only for seq > the
  committed `current_seq`. So a resume never duplicates, and a seq below the snapshot or beyond the
  log is `Diverged`. A rerun of a completed document writes nothing: no claim (the fencing token is
  unchanged), no upload. Each per-document `ImportOutcome` gives Created, Resumed or
  AlreadyComplete, per-stage flags, and `events_appended` / `events_already_present`. `stop_after`
  stops after a named stage. The error enum is fixed and leaks nothing. The probe
  (tests/legacy_import.rs, fixture tenant ad000000-…-0001, legacy ids with a per-run suffix,
  uploaded objects deleted afterwards) writes a synthetic directory in the real FCSN/FCLG format,
  then scans, plans and imports it. It checks that an unapproved plan is refused and the blocked
  document (no links.json) is skipped; that the ready document gets 3 events which replay to the
  legacy batches, plus a verified object and its mapping; that a rerun is AlreadyComplete with the
  same fencing token; that after a crash past the snapshot stage with 2 of 5 events appended
  externally, the rerun appends 3 with 2 already present, and the run after that is AlreadyComplete;
  and that a changed log gives LegacyChanged and a removed snapshot gives LegacyUnreadable. The
  section is in docs/saas/filesystem-migration-report.md under "Import and cutover verification".
  Fixture files: deploy/saas/legacy-import-{preflight,cleanup}.sql.
  Follow-up (2026-10-17): versioned legacy libraries and plugin bundles are imported idempotently
  and verified by `verify_import`; share links stay deferred until share tokens are exchanged (the
  core slice adds the table).
- [x] **S3-019** Verify migrated snapshots, logs, links, libraries and plugins before cutover.
  Evidence: `legacy_import::verify_import(tx, store, plan, root, limits)` is read-only. It reads
  verification states without changing them, takes a tenant transaction that the caller rolls back,
  and re-reads PG, S3 and the legacy directory. Each planned document gets `passed` and sorted fixed
  `VerifyCode`s: legacy_unreadable/changed, document_missing/mismatch, mapping_missing/mismatch,
  sequence_mismatch, snapshot_row_missing, snapshot_not_verified, current_snapshot_mismatch,
  object_missing, object_not_verified (key, size or digest differ from the legacy-derived values),
  object_bytes_mismatch (stored bytes do not hash to the row), snapshot_content_mismatch (stored
  bytes differ from the legacy SessionState body), store_unavailable, event_count_mismatch (count
  and min/max of rows after the snapshot against the plan's range), replay_corrupt (read_events
  finds a bad checksum, bad decoding or a gap) and replay_mismatch (replayed batches differ from the
  legacy log). Replay is bounded by the planned count. `not_imported` lists links, planned
  libraries, plugins, deferred items and excluded documents, and `cutover_ready` is true only when
  every document passed and that list is empty. Probes: a clean import with links verifies, but
  cutover is blocked by Links and ExcludedDocument; before import the only failure is
  DocumentMissing; a links-free document gives cutover_ready true. Tampering is detected: deleting
  event seq 5 as admin gives [EventCountMismatch, ReplayCorrupt]; overwriting the snapshot object's
  bytes in S3 at the same size gives ObjectBytesMismatch and SnapshotContentMismatch; a legacy log
  changed after import gives LegacyChanged.
- [x] **S3-020** Add backup/restore integration test covering database and bucket versions.
  Evidence: crates/cloud-storage/tests/backup_restore.rs: the admin URL creates the scratch database
  figcraft_backup_source_ae (static SQL) and migrate_database runs on it. Using the main APIs, the
  test then creates a principal, a tenant via create_tenant plus a member, a workspace, 2 projects
  and a document with a per-run random id. It commits 3 events through
  claim_sequencer/append_events, stores a snapshot object with unique bytes under keys::snapshot
  (register_object, put_object, verify_object reaching Verified), and adds a verified snapshot row
  set as current. Backup: the test first checks that its list of 30 tables equals every public table
  except _sqlx_migrations, so a table added by a later migration fails the test instead of going
  unrestored. It then exports each table from one REPEATABLE READ READ ONLY transaction (the same
  snapshot behaviour as pg_dump) as a bounded json_agg(row_to_json) with at most 10k rows per table,
  using literal macro-built SQL. For the bucket it signs a ListObjectVersions request with
  rusty_s3::signing::sign (query parameters must be passed pre-sorted) and records (key, versionId,
  ETag, Size, IsLatest), asserting a single real version id. Mutations after the backup: 2 more
  events, an overwrite of the object key (creating a new version), a deleted project row and a newly
  created document. Restore: a second database, figcraft_backup_restore_ae, is migrated and
  check_schema_version must equal the source version. The rows are loaded in foreign-key order in
  one transaction with json_populate_recordset. Documents load without current_snapshot_id and get
  it back after the snapshots, so every foreign key is really checked. Assertions: read_events
  replays exactly batches 1..3, current_seq is 3, and after_seq 4 gives SequenceConflict{current:3}.
  The deleted project is back, the late document is absent and current_snapshot_id is restored. The
  object row is still Verified. The key's current bytes no longer match its sha256, but GET
  ?versionId=<backed-up> returns the original bytes, matching sha256 and size_bytes, and that
  version is still listed as noncurrent with the same ETag. Re-putting those bytes makes reads by
  key match again. A fresh claim_sequencer resumes at seq 3 with the backed-up fencing token + 1,
  and the next append continues at seq 4 (rolled back). Cleanup runs whatever the outcome: every
  version and delete marker under the per-run prefix is deleted, the test asserts the listing is
  empty, and both scratch databases are dropped. Fixtures:
  deploy/saas/backup-restore-{preflight,cleanup}.sql. The procedure is documented in the new "Backup
  and restore" section of docs/saas/deployment-operations.md (pg_dump/pg_restore commands, version
  manifest, restore order, what must match). Passes twice in a row, single-threaded.

### S3 gate

- [x] **S3-GATE** Verify cloud resources require no local persistent container directory, corrupt
  objects cannot replace verified state, abandoned staging data expires and migration is repeatable.
  Evidence: Each criterion has automated evidence. No local persistent container directory:
  `apps/figcraft-cloud/tests/compose_persistence.rs` parses `deploy/saas/compose.yaml` and fails if
  any service other than PostgreSQL, Redis and SeaweedFS mounts a volume (the gateway mounts only
  its read-only Caddyfile), and every stored type (snapshots S3-008, assets S3-009, libraries
  S3-010, plugins S3-011, exports and thumbnails S3-012) is written through `ObjectStore` and
  recorded as an `objects` row. Corrupt objects cannot replace verified state: `verify_object`
  (S3-003) marks a row verified only after reading back matching size and SHA-256, rejected rows
  never return to pending, upload completion (S3-005) rejects mismatched or invalid bytes before
  promotion, and snapshot restore (S3-008) re-verifies on read and falls back past a corrupted
  object, proven by flipping stored bytes. Abandoned staging data expires:
  `uploads::expire_reservations` (S3-004) expires overdue reservations, garbage collection (S3-015)
  deletes their staging keys including every version, and S3-014 tags staging uploads and installs a
  lifecycle backstop. Migration is repeatable: the S3-018 probe reruns a completed import as
  `AlreadyComplete` with an unchanged fencing token and resumes a crash after the snapshot stage
  without duplicating events, and S3-019 verifies the result read-only before cutover. S3-020
  restores a database backup and earlier bucket versions and reopens the document to the backed-up
  sequence. Limits are recorded in `CODEX.md` (SeaweedFS lifecycle enforcement unverified, so the
  sweep is authoritative; libraries, links and plugins are not yet imported and block cutover).

## Core slice — browser co-editing on the cloud stack (pulled forward)

The product owner reprioritized on 2026-10-17: before the full S4, S5 and S6 hardening, two
browsers must co-edit one cloud document through the Compose stack, with every confirmed edit
durable in PostgreSQL and object storage and surviving a cloud restart. These tasks pull the
minimum of S4-005 (share links), S5 (a single relay serving rooms) and S6 (browser delivery)
forward; the full milestones keep their tasks and replace this slice's shortcuts.

- [x] **CS-001** Store document share links as hashed secrets with role, expiry and revocation.
  Evidence: `20261017000004_share_links.sql`.
- [x] **CS-002** Serve the collaboration protocol from the cloud relay role on PostgreSQL/S3
  durability (commit before broadcast, snapshot-first recovery) with share-link admission.
  Evidence: `figcraft-cloud-relay::collab_backend::CloudBackend` implements `collab_net::Backend`
  behind the same hub `figcraft-server` runs: `admit` resolves a hashed share link
  (`figcraft-cloud-storage::share_links`) and maps editor/viewer to edit/view access, `open` runs
  snapshot-first `DurableRoom::recover` over a deterministic base (storing a seq-0 snapshot on
  first open), `flush` appends accepted batches under the fencing token and commits before the hub
  sends anything, and failed commits close the file unconfirmed. Every cloud role serves it on port
  8082 and the gateway routes `/ws/v1/collab`. `figcraft-cloud --create-share-link` bootstraps a
  single-tenant development document and prints the editor URL. Loopback hub probes against
  PostgreSQL and SeaweedFS prove edits are durable before confirmation, a viewer sees them, a new
  hub recovers committed state, and an outside takeover closes the connection without confirming.
- [x] **CS-003** Serve the browser editor from the stack and join a cloud document from a link.
  Evidence: `deploy/saas/web.Dockerfile` builds `apps/figcraft-web` for wasm32 with pinned trunk
  and wasm-bindgen and serves it at `/edit/`; the editor reads `#join=<invite link>` at startup,
  joins through the normal join path and clears the fragment so the secret leaves the address bar.
  Unit tests cover the hash parser with a 20,000-input junk sweep; the image build and `/edit/`
  content types were checked. `docs/saas/core-slice.md` describes the demo.
- [x] **CS-004** Demonstrate two browsers co-editing through the gateway, with the document
  surviving a cloud container restart.
  Evidence: on 2026-10-17 a disposable Compose stack (`figcraft-demo`) built from `main` served
  the editor at `/edit/` (200, `application/wasm`), upgraded `/ws/v1/collab/` through the gateway
  (`101 Switching Protocols`) and `figcraft-cloud --create-share-link` created a document link;
  the product owner opened it in two browsers and confirmed live co-editing: remote cursors move in
  real time and edits appear in the other browser. Known gap: a drag is sent when the command
  completes (on drop), not while dragging, because the protocol syncs committed batches; live
  in-progress transforms need an ephemeral channel beside presence (follow-up).
- [x] **CS-005** Make branding (product name, colours, logo, favicon, links) and deployment
  configuration settable from Docker environment variables, with no code change or image rebuild.
  Evidence: landed in e20b5cd. The web image now ships deploy/saas/branding.json.tmpl as
  /etc/caddy/branding/branding.json. web.Caddyfile renders it on each request to /branding.json with
  Caddy `templates` (mime application/json). The response has Content-Type application/json;
  charset=utf-8, Cache-Control no-cache and X-Content-Type-Options nosniff, with ETag and
  Last-Modified stripped. Values come from FIGCRAFT_BRAND_PRODUCT_NAME, SHORT_NAME, PRIMARY_COLOR,
  ACCENT_COLOR, LOGO_URL, FAVICON_URL, SUPPORT_URL, TERMS_URL, PRIVACY_URL and FOOTER_TEXT. Each is
  emitted through sprig `default` plus `toJson` (Go encoding/json), so quotes, backslashes,
  newlines, `<`/`>`/`&` and invalid UTF-8 can only produce one valid JSON string. Defaults are
  FigCraft / FigCraft / #b7df79 / #94b85c, with the rest empty, which matches today's look. An
  optional read-only ./brand:/srv/brand:ro mount (commented out in compose) is served at /brand/*
  with nosniff, no-cache and a sandboxing CSP. A missing file returns 404, never the SPA fallback.
  compose.yaml passes FIGCRAFT_BRAND_* only to the web service, because the cloud config rejects
  unknown FIGCRAFT_* keys. env.example lists the defaults, with colours quoted. The SaaS shell
  (webapp/saas/src/branding.ts) fetches /branding.json with a 2 s timeout and a 16 KiB cap and
  validates every field. Names are cleaned of controls, bidi and zero-width characters, whitespace
  is folded, and lengths are capped at 64/16/200 code points. Colours must match ^#[0-9a-fA-F]{6}$.
  URLs must be a same-origin path starting with '/' (not '//'), with no whitespace or backslash, or
  an https:// URL with a host and no credentials. Invalid fields fall back to their defaults. The
  shell renders after branding loads, so there is no flash of the default name. It sets
  document.title, the meta description, the --brand-primary and --brand-accent CSS tokens, the
  header name and logo, the favicon, and footer links and text. The editor
  (apps/figcraft-web/src/branding.rs, pure and host-tested with a junk sweep) fetches /branding.json
  through web-sys and js_sys::Reflect, so no new web-sys features or Cargo.toml change were needed.
  It applies the document title and the loading and failure text, set via textContent and never
  innerHTML. Its static index.html now says only "Loading…". The editor's in-canvas accent is not
  branded because crates/ui-egui exposes no public hook. Docs are in
  docs/saas/branding-and-configuration.md: a variable table with default and validation for each, a
  pointer table of the cloud env vars, and trademark and asset notes. No asset files were added.
  Smoke test on the built image: an env value of `Acme "Design" \ </script>` plus a newline, ü and
  an emoji round-trips through json.loads to the exact literal string, with no raw </script> in the
  body. With no env set, the defaults are returned. /brand/missing.svg returns 404; / and /edit/
  return 200.

## S4 — Identity, tenancy and authorization

### Authentication

- [x] **S4-001** Implement OIDC discovery, key rotation and authorization-code flow with PKCE.
  Depends on S2-GATE.
  Evidence: OIDC login is implemented with openidconnect 4.0.1 in crates/cloud-auth/src/oidc.rs. The
  HTTP client in oidc_http.rs is an openidconnect AsyncHttpClient built on the workspace reqwest
  with tls_backend_preconfigured over an explicit rustls ring config and webpki roots. It follows no
  redirects, uses no proxy, has 5s connect / 15s request timeouts and caps response bodies at 1 MiB.
  Every request passes an endpoint policy: https only, with plain http allowed only to a loopback
  host when allow_insecure_loopback is set for tests; URLs with credentials or fragments are
  refused. OidcProvider::discover(OidcConfig) validates the config and fetches discovery and JWKS.
  The discovered issuer must equal the configured one, and the authorization, token and JWKS
  endpoints must pass the same policy. begin_login(pool, return_to) makes a fresh 256-bit state,
  nonce and PKCE verifier from ring SecureRandom and stores only SHA-256(state) in
  oidc_login_attempts, with nonce, verifier, issuer and return_to, for 10 minutes. It returns
  BeginLogin { authorization_url, state_cookie }; the URL is response_type=code with an S256
  challenge and openid plus the configured scopes. return_to is limited to relative paths: printable
  ASCII, a leading '/' with no '//' or backslash, and it must resolve to the same origin.
  complete_login(pool, code, state, state_cookie) compares state to the cookie in constant time,
  then consumes the attempt with one conditional UPDATE … RETURNING, so a state works once, before
  expiry and only for its issuer. It redeems the code with the PKCE verifier and validates the ID
  token with a public-client verifier: the alg allowlist is RS256/ES256 only, so none and HS* are
  rejected; issuer and audience must match with no other audiences; the nonce must match; exp is
  allowed 60s skew, iat may not be in the future or older than 10 minutes plus skew, and nbf is
  checked separately because openidconnect doesn't check it. Display name and the bounded email,
  email_verified, name and preferred_username claims are cleaned. A NoMatchingKey failure triggers
  one rate-limited re-discovery (key_refresh_min_interval, default 60s) and a retry. The result is
  LoginOutcome { principal_id, new_principal, return_to }. crates/cloud-storage/src/oidc_logins.rs
  maps (issuer, subject) to a principal with resolve_identity: a first login inserts a 'user'
  principal and its identity_links row in one transaction, with ON CONFLICT handling for a
  concurrent first login; an existing link refreshes verified_metadata (a JSON object of at most
  4096 bytes); a principal that isn't active gives PrincipalInactive. It also provides
  insert_login_attempt, consume_login_attempt and a bounded delete_expired_login_attempts. All
  errors are fixed non-leaking enums (OidcError::code() maps them to ErrorCode), and Debug output
  redacts secrets. The 10 DB-backed tests listed in tests_run pass against a loopback mock provider
  and the dev Postgres, covering the happy path, wrong nonce, wrong audience, expired token, nbf in
  the future, replayed state, state expiry, an unknown kid triggering a rate-limited key refresh,
  and alg none/HS256 rejected. HTTP routes and the cookie are not wired in this task (A1/integrator
  work).
  Follow-up (2026-10-17, HTTP wiring): the HTTP wiring of OIDC login is done in
  apps/figcraft-cloud/src/api/auth_routes.rs. - GET /api/v1/auth/login?return_to= checks return_to
  with return_to_allowed and calls OidcProvider::begin_login. It answers 303 to the provider and
  sets __Host-figcraft_oidc=<state>; Max-Age=600; Path=/; Secure; HttpOnly; SameSite=Lax. - GET
  /api/v1/auth/callback?code&state calls complete_login against that cookie. A wrong or missing
  cookie, or a replayed state, gives 401. On success it creates a session, clears the state cookie
  and answers 303 to the validated return_to (default /). - AppState holds Arc<OidcProvider>
  (with_oidc). health.rs discovers it at startup with a 30 s timeout when OIDC_ISSUER,
  OIDC_CLIENT_ID and OIDC_REDIRECT_URL are all set (OIDC_REDIRECT_URL is a new config key).
  Otherwise login answers 503. - The end-to-end oneshot test
  (oidc_login_callback_session_logout_end_to_end) runs against an in-test ES256 mock provider that
  checks PKCE, and passes on the dev DB. - The OIDC core work was already evidenced. This paragraph
  adds only the routes.
- [x] **S4-002** Implement secure browser sessions, rotation and logout.
  Evidence: crates/cloud-storage/src/sessions.rs + crates/cloud-auth/src/session_policy.rs. A
  session token is 32 bytes from ring SecureRandom, written as base64url with no padding (43 chars).
  It is returned once (Debug redacted), and only its SHA-256 goes into browser_sessions.token_hash.
  create_session works only for an active principal. resolve_session refuses unknown, malformed,
  revoked, idle-expired, absolute-expired and inactive-principal tokens with the same
  SessionError::Refused. It slides the idle expiry, capped at the absolute expiry, writing at most
  once per touch interval (default 60s) through a guarded UPDATE. rotate_session takes FOR UPDATE on
  the old row, revokes it with 'rotated', and inserts a new row linked by rotated_from that keeps
  the original absolute expiry, so rotation never extends a session. Replaying a rotated token is
  refused, and of two concurrent rotations exactly one wins. Also: logout (idempotent),
  revoke_all_for_principal(reason, capped at 10k), and a revoke_expired_sessions sweep.
  SessionPolicy defaults to 8h idle, 7d absolute, 1h rotation interval and 60s touch. new()
  validates the ranges, rotation_due(age, Login|PrivilegeChange) decides rotation, and set_cookie
  gives `__Host-figcraft_session=<tok>; Max-Age=..; Path=/; Secure; HttpOnly; SameSite=Lax` with no
  Domain; it refuses malformed tokens, so no header injection. There is also clear_cookie, plus a
  strict cookie-header parser: 8 KiB cap, and a duplicate cookie means None. Dev-DB tests cover
  hash-only storage, expiry via backdating, bounded slide, the rotation chain and replay, the
  concurrent-rotation race, suspend/reactivate, logout and revoke-all isolation.
  Follow-up (2026-10-17, HTTP wiring): browser sessions now work over HTTP. -
  CredentialAuthenticator reads __Host-figcraft_session (it must appear exactly once across all
  Cookie headers, at most 8 KiB, URL-safe) and calls sessions::resolve_session with SessionLimits
  derived from SessionPolicy. - Rotation on login: the callback revokes any session the browser
  already presented before creating a new one. The test shows the old cookie gets 401 afterwards. -
  Age rotation: GET /auth/session rotates a session past the rotation interval and sets the new
  cookie. - POST /auth/logout is same-origin only, revokes the session and clears the session and
  share cookies (204). The token is dead afterwards. - CSRF: cookie-authenticated requests with
  unsafe methods need Origin equal to the FIGCRAFT_PUBLIC_URL origin (normalized), or
  Sec-Fetch-Site: same-origin when there is no Origin. If neither is present the request is refused.
  The test checks no Origin, an evil Origin, Origin null and a cross-site Sec-Fetch-Site all get
  403, while GET, our Origin and same-origin pass. - A request in a tenant needs active membership
  (resolve_tenant_access in a tenant transaction). Another tenant, an unknown tenant and a
  mismatched X-Figcraft-Tenant header all get the same 403. - Not done: per-request Set-Cookie
  rotation on every route.
- [x] **S4-003** Implement single-tenant bootstrap administrator with forced bootstrap completion.
  Evidence: crates/cloud-storage/src/bootstrap.rs. ensure_bootstrap(tx, lifetime ≤ 7d) locks the
  tenant row and issues a one-time token (only its hash is stored). Calling it again while pending
  replaces the token, so a superseded token is refused, and once the tenant is done it returns
  Completed with no token. complete_bootstrap(tx, token, principal) compares hashes in constant time
  against the locked row and checks expiry. The principal must be an active user: service, pending
  and unknown principals are refused. It upserts the principal as an active tenant_owner, marks the
  row completed_by, and writes the audit event tenant.bootstrap.complete. bootstrap_required(pool,
  tenant) stays true until a bootstrap is completed or an active owner exists (a suspended owner
  does not count); this gives the API forced bootstrap completion. Tests cover one-time use,
  superseded, expired, junk and cross-tenant tokens, invalid principals, an existing owner, and an
  unknown tenant.
  Follow-up (2026-10-17, HTTP wiring): forced bootstrap now works over HTTP. -
  auth_routes::bootstrap_gate is layered over all of /api/v1. In single-tenant mode, while
  bootstrap_required is true, every route except auth/login, callback, logout, session and
  bootstrap/complete answers a fixed 403 with
  {"error":{"code":"bootstrap_required",…,"request_id"}}. This includes unknown paths and
  share/exchange. A completed bootstrap is remembered in memory. - POST /auth/bootstrap/complete
  {"token"} is same-origin only. It needs a browser session, runs complete_bootstrap in a tenant
  transaction, then rotates the session (privilege change). Without a session it gives 401; a wrong
  or replayed token gives 403; it gives 404 in multi-tenant mode. - The test passes on the dev DB up
  to its last assertion. That last assertion (a fresh replica sees bootstrap done) needs B4's
  RLS-safe bootstrap_required.
- [x] **S4-004** Implement service accounts with scoped, expiring and rotatable credentials.
  Evidence: crates/cloud-storage/src/service_accounts.rs. The fixed Scope allowlist is
  documents:read, documents:write, exports:create and jobs:read. parse_scopes rejects anything off
  the list, empty input and more than 64 entries, and it dedups; it has a junk sweep with a
  deterministic 2000-case fuzz. create_service_account makes a service principal plus a
  service_accounts row and audits it. issue_credential creates an `fcsa_`+43-char secret, returned
  once and stored hash-only, with expiry ≤ 1 year and at most 5 live per account.
  authenticate_credential(pool, secret) returns tenant, principal, account, credential and scopes.
  It checks revocation and expiry, and requires the account, principal and tenant to be active.
  Stored scopes that are off the allowlist are dropped, and an account left with none is refused. It
  updates last_used_at at most once per 60s. rotate_credential issues a new secret and shortens the
  old one to an overlap of 0..7d (0 revokes it at once). revoke_credential and
  set_service_account_enabled (disable/enable) are audited and tenant-scoped. Tests cover every
  path, including caps, cross-tenant NotFound, a disabled account, a suspended principal, and the
  rotation overlap and expiry.
  Follow-up (2026-10-17, HTTP wiring): service-account bearer credentials now work over HTTP. -
  Authorization: Bearer fcsa_… goes to authenticate_credential. Note: the real prefix is fcsa_, not
  fc_sa_ as the task said. - Any other Authorization value is 401 before storage is touched. - The
  result is Authenticated{grant: Grant::ServiceAccount{scopes}}, bound to its own tenant: another
  tenant gives 403. Handlers enforce scopes with Authenticated::require_scope. Bearer requests are
  exempt from CSRF. - /auth/session reports the scopes and never the secret, and a revoked
  credential gives 401 at once. - That DB test needs B4's figcraft_service_credential storage change
  under the RLS now on the dev DB.
- [x] **S4-005** Implement share-token generation, hashing, exchange, expiry and revocation.
  Evidence: share links can now be generated, hashed, exchanged, given an expiry and revoked.
  Generation, SHA-256-only storage and revocation came from C1. This change adds
  exchange_share_token(pool, secret) -> ShareGrant{link_id, tenant_id, document_id, role,
  created_by, link_expires_at, session_expires_at} in crates/cloud-storage/src/share_links.rs.
  session_expires_at is computed in SQL as the earlier of the link's expiry and
  now()+MAX_SHARE_SESSION_SECS (1 h). The grant never carries the secret. Exchange shares one query
  with resolve_share_link, so both refuse the same things with a single Refused: unknown, malformed,
  revoked or expired links, a document that is not active, and (new) a tenant that is not active.
  Also added: list_share_links (tenant-scoped, newest first, capped at 500, includes revoked links,
  never returns secrets or hashes); update_share_link, which changes or clears the label and resets
  or clears the expiry on live links only, validates its input and returns None across tenants; and
  ShareLinkRecord::live_at. Tests (prefix c6): the session end is bounded by the link expiry and by
  1 h; revoked, malformed and archived-document links are refused; the Debug output holds no secret;
  list and update are tenant-isolated, including bad-input cases. The API that mints share sessions
  from a ShareGrant is not part of this task. partial
  Follow-up (2026-10-17, HTTP wiring): the HTTP exchange of share tokens is done. - POST
  /auth/share/exchange {"token"} is same-origin only and calls exchange_share_token. It sets
  __Host-figcraft_share, SameSite=Strict, with Max-Age = session_expires_at - now (at most 3600). -
  The cookie is an HMAC-SHA256-signed (link, tenant, document, role, creator, expiry) tuple. It
  never contains the share secret. - The MAC is keyed by HMAC(SESSION_SIGNING_KEY, label), or by a
  random per-process key when that is not set. It is checked with ring::hmac::verify. A forged role,
  expiry or document is refused, and so is a cookie signed with another key. - On every request the
  link is re-checked with dead_share_links, so revoking it ends the session on the next request. -
  The authenticator refuses a share session on any route that does not name its document (403).
  require_document and require_scope (documents:read, plus documents:write for editor links) are
  there for handlers. - That DB test needs B4's figcraft_live_share_link and
  figcraft_dead_share_links changes under the current RLS.
- [x] **S4-006** Add authentication rate limits and safe failure responses.
  Evidence: apps/figcraft-cloud/src/api/rate_limit.rs adds a token-bucket limiter keyed by client IP
  and matched route template. IPv6 is grouped by /64 and IPv4-mapped addresses count as IPv4. The
  default is a burst of 10 with one token back every 6 s. Memory is bounded at max_keys 10,000: idle
  (refilled) buckets are evicted first, then the least recently used. It is installed as middleware
  only on the /api/v1/auth router (and its fallback), so every auth endpoint merged there is limited
  and resource routes are not. A refused request gets 429 with the rate_limited envelope and
  Retry-After rounded up to whole seconds (at least 1), and no detail about the limit. The client IP
  comes from api/client_ip.rs: the socket peer, unless the peer is inside
  FIGCRAFT_TRUSTED_PROXY_CIDRS (new config key, default none, at most 64 CIDRs, validated). Only
  then is X-Forwarded-For walked from the right, with header size and hop count capped, stopping at
  the first untrusted address or at garbage. Tests show: burst then 429 then refill; separate
  buckets per client and per route; rotating XFF without a trusted proxy does not escape the limit;
  behind a trusted proxy each forwarded client gets its own bucket; a client cannot choose its
  bucket by adding entries on the left.

### Tenant and role model

- [x] **S4-007** Implement tenant creation, suspension and plan assignment.
  Evidence: crates/cloud-storage/src/tenants.rs adds create_tenant_with_owner. It validates the
  slug: lowercase alphanumerics with single interior hyphens, at most 63 characters, no leading or
  trailing hyphen, and no reserved route names such as admin/api/auth/oidc/scim/www. It also
  validates the name (trimmed, 1-255 characters, no control characters), the region and the plan. In
  one TenantTransaction it then reuses metadata::create_tenant, adds the tenant_owner membership
  (the owner must be an active user principal, otherwise OwnerUnavailable and the transaction is
  rolled back with no tenant row) and records the tenant.create audit event. A taken slug returns
  Conflict. suspend_tenant (active to suspended, idempotent, optional bounded reason, audit
  tenant.suspend) and reactivate_tenant (suspended to active, idempotent, audit tenant.reactivate)
  lock the tenant row FOR UPDATE and refuse deleting/deleted tenants with InvalidState. assign_plan
  checks the placeholder PLAN_ALLOWLIST (free/starter/team/business/enterprise/self_hosted) and
  works while active or suspended; assigning the current plan is a no-op, and a change records
  tenant.plan_assign with from/to. list_tenants_for_principal is a pool-level keyset page by tenant
  id (PageSize max 100, has_more probe). It returns only active memberships with the member's role,
  leaves out deleting/deleted tenants and keeps suspended tenants with their state. Dev-DB tests
  cover every validation error, the owner rollback, the slug conflict, idempotent transitions with
  exact audit counts, deleting-state refusal and two-page listing that leaves out suspended
  memberships. Metadata helpers (TenantRow, tenant, state, page, PageSize::query_limit/as_usize)
  became pub(crate) only; no behaviour change.
- [x] **S4-008** Implement tenant membership and owner/admin roles.
  Evidence: memberships.rs provides add_tenant_member (re-adds deleted members), get/list (bounded
  at 100, keyset), change_tenant_member_role, suspend/reactivate and remove (soft-delete; drops
  direct memberships, team memberships and permissions). Last-owner protection locks the tenant row
  FOR UPDATE, then counts other active owners. DB tests prove the last owner cannot be demoted,
  suspended or removed, can step down once a second owner exists, and that removal cascades. Role
  boundaries are validated (InvalidRole).
- [x] **S4-009** Implement teams, invitations, team membership, team-manager role and team ownership
  or grants for workspaces, projects and libraries.
  Evidence: crates/cloud-storage/src/teams.rs adds create_team, which makes the creator (an active
  tenant member) the first team_manager, plus get/list (keyset), rename, archive (idempotent) and
  add/remove/change-role for members. Each membership change locks the team row FOR UPDATE, so the
  rule that a team keeps at least one active team_manager cannot be raced (LastManager on removing
  or demoting the last one). Removed members can be re-added. Archived teams refuse new members,
  ownership and grants, but grant revocation still works. set_owning_team sets or clears
  owning_team_id on workspaces, projects and libraries; grant_to_team upserts workspace_team_grants
  and library_team_grants; revoke_team_grant removes them. All of these record audit events: team.cr
  eate/rename/archive/member_add/member_remove/member_role/ownership_set/ownership_clear/grant_set/g
  rant_revoke. A team id from another tenant returns NotFound.
  crates/cloud-storage/src/invitations.rs: create_invitation makes a 32-byte ring SecureRandom
  token, encoded as 43 URL-safe base64 characters and returned once (Debug redacts it). Only its
  SHA-256 is stored. The email is normalised, expiry defaults to 7 days (at most 30), and the tenant
  and team must both be active. It records invitation.create without the email.
  accept_invitation(pool, token, principal, verified_email) looks the invitation up by hash. In one
  tenant transaction it locks the pending row FOR UPDATE (under READ COMMITTED the predicate is
  re-checked after the wait, so exactly one concurrent accept wins), requires an active tenant
  (TenantInactive otherwise), an active team and an active user principal, and an optional
  verified-email match. It then adds or reactivates the tenant membership as viewer (an existing
  active member keeps their role; a suspended one is refused), adds the team membership, marks the
  invitation accepted and records invitation.accept, all committed together. Unknown, malformed,
  expired, revoked and already-accepted tokens all return the same Refused error and change nothing.
  revoke_invitation records invitation.revoke; list_pending_invitations is keyset-paged and never
  returns the token. Dev-DB tests cover every one of these paths, including 4 concurrent contenders
  over 3 rounds with exactly one winner (one team member, one audit row), and a suspended tenant
  refusing acceptance that works again after reactivation.
- [x] **S4-010** Implement direct workspace/document membership, inherited team grants and
  workspace-admin/editor/commenter/viewer roles.
  Evidence: set/remove workspace and document direct membership (active tenant members only; roles
  validated per boundary), and set/remove team grants on workspaces and documents (the team must
  exist in the tenant). Inheritance to projects and documents is covered by DB tests. Team roster
  CRUD is S4-009 and was not done here; the tests write team_members directly.
- [x] **S4-011** Implement explicit sensitive permissions for billing, provider keys, support,
  export and deletion.
  Evidence: SensitivePermission in authz (billing, provider_keys, support, export, deletion) and
  MemberPermission with grant/revoke/list in storage. Decision, documented in the authz module
  header: no role ever implies a sensitive permission, not even tenant_owner, following
  security.md's 'separate explicit permission'. billing.manage, tenant.export and tenant.delete need
  tenant_owner plus the grant. provider_keys.manage, support.grant and tenant.export_audit need
  tenant_owner or tenant_admin plus the grant. Granting permissions (member.manage_permissions) is
  owner-only and needs no sensitive grant, so a new owner can bootstrap. Unit tests show the owner
  is denied without the grant, a different grant does not substitute, and a grant never lifts an
  insufficient role. DB tests show the owner starts with no permissions, grant/revoke are
  idempotent, and members who are not active cannot be granted. Partial
- [x] **S4-012** Implement effective-role resolution and deny-by-default authorization checks.
  Evidence: crates/cloud-auth/src/authz.rs is a pure, deny-by-default policy with no storage access.
  It defines an Action enum of 44 API and relay actions across the tenant, team, workspace, project
  and document scopes:
  tenant.view/update_policy/view_audit/export_audit/export/delete/transfer_ownership,
  member.view/manage/manage_owners/manage_permissions, billing.manage, provider_keys.manage,
  support.grant, service_account.manage, team.create/view/manage,
  workspace.create/view/update/manage_members/archive/restore/delete,
  project.create/view/update/archive/restore/delete, document.create/view/join_realtime/presence/com
  ment/edit/create_version/export/share/rename/archive/restore/delete. Each action has a stable
  dotted id. role_allows(role, action) is the matrix, and decide(&Subject, &Resource, Action)
  returns Decision{allow, reason} with fixed Reason values. decide checks, in order: wrong scope,
  unsupported action (transfer_ownership has no permission value yet, so it is denied for everyone),
  principal inactive, tenant not active, no active tenant membership, resource state (archived
  allows only view/export/restore/delete; deleting allows only restore; pending/suspended/deleted
  allow nothing), role, then the sensitive grant. crates/cloud-storage/src/effective_roles.rs
  provides resolve_tenant_access, resolve_workspace_access, resolve_project_access,
  resolve_document_access and resolve_team_role. The effective role is the strongest of four
  sources: the tenant baseline (owner/admin act as workspace_admin), direct workspace_members,
  workspace_team_grants through an active membership in an active team (capped by the member's team
  role), and document_members/document_team_grants. A commenter or viewer tenant role caps every
  resource role. The resource state is the most restrictive along document->project->workspace.
  Resolution reports which sources supplied the role, for audit. A principal that is not active, a
  tenant that is not active, or a tenant membership that is not active yields no role and no
  permissions. DB tests prove: a team grant gives editor; a direct viewer membership does not
  downgrade it; removing the team membership removes inherited access; a suspended team grants
  nothing; a suspended tenant denies even the owner.
- [x] **S4-013** Recheck authorization for long-running jobs before committing results.
  Evidence: crates/cloud-storage/src/job_authorization.rs adds recheck_before_commit(tx,
  &JobAuthorization{requested_by, document, permission: DocumentExport|DocumentView|DocumentEdit}).
  It runs inside the transaction that commits the job's result. It takes FOR SHARE locks on every
  row the decision reads (tenant, principal, tenant membership, workspace, project, document, direct
  workspace and document memberships, team memberships, teams, team grants; tenant first, like the
  membership repositories), then re-resolves resolve_document_access and applies the authz rule for
  the action. So a revocation either commits before the recheck and is seen, or waits for the job's
  commit. On loss of access it returns Revoked. AUTHORIZATION_REVOKED = "authorization_revoked", and
  fail_job_authorization_revoked fails any job kind with that code. exports::complete_export now
  calls the recheck after locking the export and before writing any bytes, and returns
  ExportError::AuthorizationRevoked; the worker rolls back and calls fail_export(tx, id,
  "authorization_revoked"). DB tests: exports whose requester lost a direct workspace membership,
  left the granting team, or was removed from the tenant after enqueue and start are refused at
  commit, then failed with export and job both in state failed with error_code authorization_revoked
  and no result object. A requester who kept access completes. A concurrent remove_workspace_member
  blocks while the recheck's transaction is open and commits after it, and the next recheck returns
  Revoked.

### Isolation

- [x] **S4-014** Enable row-level security on every tenant-owned table.
  Evidence: Migration crates/cloud-storage/migrations/20261017000006_row_level_security.sql turns on
  ENABLE and FORCE ROW LEVEL SECURITY for every public table with a tenant_id column plus tenants
  (36 tables in total). It finds them by walking the catalog, so none can be missed. Each table gets
  a tenant_isolation policy for all roles: USING and WITH CHECK (tenant_id =
  NULLIF(current_setting('figcraft.tenant_id', true), '')::uuid). The NULLIF matters because a
  pooled connection that already ran a SET LOCAL reads '' afterwards, and that must also match
  nothing. The global tables are left without tenant RLS on purpose, and the migration says so:
  principals, identity_links, browser_sessions, oidc_login_attempts, _sqlx_migrations. Lookups that
  must run before the tenant is known go through five SECURITY DEFINER plpgsql functions with
  search_path pinned to pg_catalog, pg_temp, fixed SQL only, and size and limit caps. Each returns
  only the minimal row: figcraft_live_share_link (token hash), figcraft_dead_share_links (at most
  1024 pairs, otherwise it raises), figcraft_service_credential (secret hash),
  figcraft_invitation_tenant (token hash), figcraft_principal_tenants (a principal's active tenants,
  at most 101 rows). They can read rows only through a definer_lookup SELECT policy on the 7 tables
  they touch, which admits a row only when current_user = 'figcraft_migrator' and
  figcraft.definer_lookup = 'on'. Each function sets that flag around its one query, so the runtime
  role cannot use the policy even if it sets the flag itself. The following now call these
  functions, with their public signatures unchanged: share_links (resolve, exchange,
  dead_share_links), service_accounts::authenticate_credential (the last_used_at update now runs
  inside the credential's tenant transaction), invitations::accept_invitation (tenant lookup), and
  both list_tenants_for_principal functions. bootstrap_required now reads inside a short tenant
  transaction. provision_database_roles makes figcraft_migrator the owner of the functions, revokes
  PUBLIC and grants EXECUTE to figcraft_runtime only. deploy/saas/role-preflight.sql now checks:
  every tenant-owned table has RLS enabled and forced with tenant_isolation (at least 36 tables);
  the five functions are SECURITY DEFINER, owned by the migrator, have the pinned search_path, and
  are runtime-executable but not PUBLIC; no other SECURITY DEFINER function exists; and, as the
  runtime role, there are zero tenant rows and a refused insert without a context. The new
  crates/cloud-storage/tests/row_level_security.rs (fixture prefix d4) proves: outside a tenant
  transaction every protected table returns count 0 while the fixtures exist; inside tenant A only
  A's rows appear in every protected table, and inside B only B's; the same pooled connection sees
  nothing after commit; inserting without a context, inserting another tenant's row, or moving a row
  to another tenant fails with 42501 or WITH CHECK; updates to another tenant's rows affect 0 rows;
  setting the definer flag opens nothing; the definer lookups work across tenants (listing, share
  resolve and exchange, dead links per tenant), return nothing for unknown hashes and refuse
  oversized batches; and the runtime role cannot redefine them. Applied to the shared dev DB, and
  every cloud-storage and cloud-relay DB suite passes under RLS except backup_restore and exports.
  backup_restore was already broken and is unaffected by RLS. exports is blocked by stale dev-DB
  rows; see issues.
- [x] **S4-015** Add tenant to every object, cache, lease and job key.
  Evidence: docs/saas/tenant-key-audit.md lists every object-store key family (all
  tenants/{tenant}/..., enforced by the objects CHECK and the staging_key CHECK) and every DB key,
  lock and lease: composite tenant PKs and FKs, the event log, idempotency, the sequencer fencing
  token on (tenant_id, id), and every FOR UPDATE / SKIP LOCKED filtering on tenant_id. It also
  covers jobs (random UUID with tenant_id NOT NULL, global claim index on purpose, kind is a job
  type), secret-hash keys (globally unique by design), advisory locks (none), Redis (not used yet;
  the doc sets the rule that keys are prefixed t:{tenant}:) and the relay's in-process maps (keyed
  by document UUID with the tenant recorded and checked). No key family was found that lacked the
  tenant or a globally unique id, so nothing needed fixing.
- [x] **S4-016** Normalize forbidden and not-found behavior according to disclosure policy.
  Evidence: api/error.rs adds ApiError, which always renders
  {"error":{"code","message","request_id"}} from figcraft_cloud_domain::PublicError with the fixed
  per-code message; the context middleware fills in the request ID. The module doc states the
  disclosure policy taken from docs/saas/security.md: 401 before any lookup; missing, other-tenant
  or invisible resources get 404 not_found; a visible resource without the permission gets 403
  forbidden; tenant-level admin without a resource id gets 403. Helpers authorize_resource(found,
  can_view, can_act) and denied(can_view) implement it. ApiJson<T> rejections map to 400, 413 or 415
  without echoing parser text, and bare framework 400/405/413/415/422 responses under /api/ are
  normalised into the envelope. Router tests show that an existing-but-invisible resource cannot be
  told apart from a missing one, and that a viewer-only resource gets 403. Foundation (no tracker
  id): health.rs now runs axum on 8081 with the same probe routes, bodies and status codes
  (/healthz, /startupz, /readyz, /api/v1/healthz in api roles, /ws/v1/health upgrade diagnostic in
  relay roles, legacy 404 and 405 bodies outside /api), and --check-ready still works. The context
  middleware assigns a RequestId (X-Request-Id), validates and echoes traceparent, and writes a JSON
  request log with method, matched route template, status and duration (never the raw path, query,
  cookies or authorization). It also enforces a 16 KiB header cap (431), the declared Content-Length
  cap, a 1024-request in-flight cap (503) and a 30 s deadline (503); DefaultBodyLimit defaults to 1
  MiB. api::api_router(AppState) -> Router gives AppState holding PgPool, ObjectStore, ApiConfig,
  Arc<dyn Authenticator> and the limiter. The Authenticated { principal, tenant: TenantContext,
  session: SessionInfo } extractor is resolved by the Authenticator trait, with DenyAll as the
  default and a fake used in tests.
- [x] **S4-017** Close active sessions after direct membership, team membership, team grant,
  share-link or tenant revocation.
  Partial (2026-10-17): share-link, archived-document and suspended-tenant revocation now close live
  relay sessions; membership, team and grant revocation do not yet. dead_share_links(pool,
  &[(tenant, link)]) is bounded at 1024 links and returns the links that no longer open anything:
  revoked, expired, deleted, wrong tenant, document not active, or tenant not active. In
  crates/cloud-relay/src/collab_backend.rs, CloudBackend records every admitted (file, tenant, link)
  in a RevocationWatch, capped at 256 links per file (a hello beyond that is refused as
  UNAVAILABLE). RevocationSweeper::start(config, watch, hub_addr, interval), default
  REVOCATION_INTERVAL of 5 s and minimum 50 ms, re-checks the watched links on its own DB connection
  and thread. It sends Admin::Revoke for each dead link over the hub's managing connection, using a
  random in-process key compared in constant time. Backend::admin checks the link against storage
  again before it returns Handled.revoked, so the key cannot disconnect anyone whose link still
  works. The hub then closes those clients with STOPPED, and new hellos are refused by admit. The
  relay test (c6) shows: a live link survives several sweeps; once the link is revoked, its editor
  is closed with STOPPED in under 5 s at a 100 ms interval; a new hello with that link gets REFUSED
  while the other link still joins; archiving the document closes the remaining clients and refuses
  new hellos. The storage test covers a suspended tenant.
  Evidence: Membership, team-membership, team-grant and tenant revocation now close live access,
  completing the share-link, archived-document and suspended-tenant work already done. Policy
  (crates/cloud-storage/src/revocation.rs module doc): browser sessions belong to a principal and
  never cache tenant scope. Every API request re-resolves tenant access;
  authorize_session_in_tenant(pool, token, limits, tenant) is the check the authenticator uses. A
  per-tenant revocation epoch was rejected because it would need a schema change for no gain.
  apply_revocation(pool, AccessRevocation::...) is called after a mutation commits. It revokes every
  session, with reason membership_revoked, of a principal left with no active membership in any
  active tenant, and every session, with reason principal_suspended, of a suspended principal. A
  principal still trusted by another tenant keeps their sessions. Team, grant and tenant-suspension
  revocations change no session. The relay now admits signed-in members with a hello token
  member.<tenant>.<session token> (admit_member: live session, effective role at least viewer on an
  active document, editor or above to edit) and records the principal, session and access. The
  RevocationSweeper re-checks member admissions with dead_member_admissions (bounded at 1024 per
  call; tenant transactions, so it works under RLS). It disconnects, over the hub's managing
  connection under the key session:<id> (re-checked in Backend::admin), any member whose session was
  revoked or expired, who lost the document from any source, or whose editor access was downgraded.
  Relay test member_revocation.rs, 100 ms sweep: a member removed from the tenant is closed with
  STOPPED in under 5 s; their next API request is refused and their next hello is REFUSED. A member
  removed from the team whose workspace grant admitted them is closed and refused, but tenant-level
  requests still work for them. A downgraded editor is closed and rejoins with View only. A member
  who kept access edits across all of this without interruption. Storage tests cover downgrade,
  removal, team removal and logout for dead_member_admissions, and the per-request refusal plus
  last-tenant sign-out.
- [x] **S4-018** Add support-access grants with reason, expiry and audit.
  Evidence: crates/cloud-storage/src/support_access.rs adds the following. grant_support_access(tx,
  granted_to, granted_by, reason, scope read|write, expires_in_secs): the reason is trimmed, 1..500
  characters, no control characters; expiry is 1 s..7 days, the same bound as the table CHECK; the
  grantee must be an active principal other than the granter. revoke_support_access(tx, grant,
  actor, reason). list_active_support_grants (bounded at 200). active_support_access (read-only, for
  display). use_support_access(tx, principal, action, resource_kind), which is the check
  authorization calls. Each grant, revoke and use writes an audit event in the same transaction
  carrying the reason: support.grant {grant, granted_to, scope, expires_at, reason}; support.revoke
  {reason, grant_reason}; support.use {grant, scope, reason, action, resource_kind}.
  SupportAccess::tenant_role() / resource_role() map a read grant to viewer and a write grant to
  editor on every resource, and never to an admin role or sensitive permission. The authenticator
  builds the operator's Subject from them. A DB test proves: every invalid input is refused; the
  grant is visible, listed and usable with the use audited; another tenant sees nothing and cannot
  revoke it; an expired grant opens nothing, is not listed, writes no audit and cannot be revoked;
  the table refuses expiry beyond 7 days; revoke is audited with both reasons, after which nothing
  opens and a second revoke is NotFound.

### API and testing

- [x] **S4-019** Implement tenant, team, invitation, workspace, project, document and member APIs.
  Partial (2026-10-17): only the auth slice of the API is done (the /api/v1/auth routes above). It
  also adds GET /auth/session, a "who am I" endpoint: credential kind, principal, tenant id, role
  and permissions or null, scopes or the shared document, bootstrap_required and deployment, and no
  secrets. A SignedIn extractor (principal with no tenant, for B2's tenant listing) is also added.
  Tenant, team, invitation, workspace, project, document and member APIs belong to B2 and B3. The
  tenant, member, team, team-grant and invitation APIs are done; workspace, project and document
  APIs are B3's. In apps/figcraft-cloud/src/api/tenants_api.rs: GET/POST /api/v1/tenants. Create
  works only in multi-tenant mode and only for browser sessions; single-tenant mode, service
  accounts and share links get 403. GET/PATCH /tenants/{id} handles rename (audited tenant.rename),
  suspend and reactivate under tenant.update_policy. Reactivation is the only change allowed while
  the tenant is suspended: it is decided on the caller's raw active membership role, and an editor
  gets 403. Member routes: GET (prefix search on display name), POST, PATCH (role/state) and DELETE
  /tenants/{id}/members. A change that involves an owner needs member.manage_owners. Last-owner
  protection answers 409. Each change records an audited member.add/update/remove. GET/PUT/DELETE
  /tenants/{id}/members/{p}/permissions/{perm} needs member.manage_permissions (owner only) plus an
  explicit grant policy: a permission can only go to an active member whose current role could use
  it under role_allows. So billing goes to owners, provider_keys to owners and admins, and nothing
  goes to editors or viewers. Grants and revokes are audited. In teams_api.rs: team CRUD (create
  needs team.create; rename and archive need team.manage); team member list, add, role change and
  remove (last-manager check gives 409); workspace team grants (PUT needs team.manage and
  workspace.manage_members; DELETE needs only the workspace admin, so it also works on archived
  teams); team ownership of workspaces and projects; invitations: create (returns the token once,
  Cache-Control no-store; needs team.manage and member.manage because acceptance adds the person to
  the tenant), list per team or tenant (the token is never shown), revoke, and POST
  /api/v1/invitations/accept (browser sessions only; passes the caller's latest verified
  identity-link email; every refusal is the same 404). Every handler goes Authenticated → share link
  403 → path tenant must be the credential's tenant (else 404) → resolve_tenant_access /
  resolve_team_role / resolve_workspace_access → authz::decide → disclosure policy: anything the
  caller cannot see is 404, a visible resource without the permission is 403, and a suspended tenant
  is 409 resource_inactive for its active members. Router tests in tenants_api_tests.rs use a
  header-driven fake Authenticator against the dev DB. They cover allow and deny per role
  (owner/admin/editor/viewer/stranger/other-tenant owner/share link/service account/no credential)
  for every route family, cross-tenant 404s on tenant, team and team-member paths, last-owner and
  last-manager conflicts, the grant policy, and audit rows. Not done: project-level team grants (the
  schema has no project grant table, so only project ownership is exposed); team-member search; and
  the HTTP routes are not yet part of the S4-021 route matrix. The workspace, project, document and
  member APIs are done in apps/figcraft-cloud/src/api/documents_api.rs, 34 routes under /api/v1, all
  listed in ROUTES with their authz Action. They are: workspaces list (pre-filtered to visible ones
  and re-decided per row), create (tenant admin), get, rename, archive, restore and soft-delete;
  direct workspace members list, put and delete; projects list, create, get, rename, archive,
  restore and soft-delete; documents list (keyset pages), create, get (returns the caller's
  effective role and can_edit), rename, archive, restore and soft-delete; direct document members;
  document team grants; share links list, create and revoke for a document (create returns the
  secret once with Cache-Control: no-store, an editor link needs document.edit, and revoke checks
  the link belongs to that document); and GET /documents/{id}/collab-link. collab-link mints an
  8-hour share secret for an authorized editor or viewer and returns access, invite
  (ws(s)://<FIGCRAFT_PUBLIC_URL host>/ws/v1/collab?file=<id>&token=<secret>) and editor_url, so the
  browser no longer needs the CLI. Every handler goes Authenticated → tenant transaction in the
  caller's tenant → effective_roles → authz::decide → disclosure policy: missing, other-tenant or
  invisible gives 404, visible but not permitted gives 403, and a tenant-scope refusal gives 403.
  Share-link credentials get 403 on all of these routes. The new storage module
  crates/cloud-storage/src/resource_admin.rs adds workspace/project rename and lifecycle
  transitions, get_project_by_id, list_documents, list_visible_workspaces, and bounded member and
  team-grant listings. End-to-end DB tests (fixture prefix d3) cover admin, editor, document viewer,
  tenant member with no grants, and another tenant's admin across document, workspace and project
  routes, lifecycle states, share-link create/list/revoke and collab-link. The tenant, team and
  invitation APIs belong to B2 and are not in this change, so S4-019 as a whole is only met once
  B2's part merges. Not done: audit records for these mutations, and workspace team-grant routes
  (the storage functions exist; no routes yet).
  Partial (2026-10-17, round C): This finishes the C1 slice. (1) Revocation after member changes. A
  new tenants_api::revoke_after_commit helper calls revocation::apply_revocation after each change
  commits: - member DELETE → TenantMemberRemoved - member PATCH to state=suspended →
  TenantMemberSuspended - tenant PATCH to suspended → TenantSuspended - team archive (PATCH or
  DELETE) → TeamArchived - team-member removal → TeamMemberRemoved - workspace team-grant DELETE →
  GrantRevoked If the follow-up fails, the change is not undone and the request still succeeds. The
  failure goes to stderr with ids only, and access is still refused on every request. Principal
  suspension has no API route, so PrincipalSuspended is not wired. The router test uses real browser
  sessions through the production CredentialAuthenticator. A removed member's next request is 401
  and both their sessions are revoked. The same holds for a suspended member. A member removed here
  who still belongs to another active tenant gets 403 here, keeps their sessions and gets 200 in the
  other tenant. The owner is untouched. (2) documents_api lists now use api/cursor.rs. The local
  CursorCodec and LocalCursor are gone. Cursors are bound to the list kind, tenant, caller and
  parent resource. Behaviour is unchanged: a bad limit is still 400 invalid_request, and a bad or
  foreign cursor is 400 invalid_cursor (it was 400 invalid_request). Tests: pure binding tests,
  end-to-end paging over 5 documents, and refusal of a workspaces cursor, another caller's cursor
  and another project's cursor. (3) Realtime tickets. They are stateless, with no migration. A
  ticket is "rt1_" + base64url of: version | tenant | principal | browser session | access | exp |
  HMAC-SHA256 tag. It is 124 characters, which fits the invite's 128-character token limit. The key
  is a subkey derived from SESSION_SIGNING_KEY. The document id is covered by the MAC but not
  carried in the ticket. Lifetime is capped at 60 s; a ticket expiring more than 60 s + 5 s skew
  ahead is refused. POST /api/v1/documents/{id}/realtime-ticket is for browser sessions only (403
  for other credentials) and answers 503 without a key. It needs document.join_realtime: 404 when
  the caller can't see the document, 403 when archived. Access is edit only with document.edit. It
  returns {ticket, expires_in, ws_url, access, document_id} with Cache-Control: no-store. The
  relay's CloudBackend::admit sends ticket-shaped tokens to admit_ticket. That checks MAC, expiry
  and document, then re-checks the live session and effective role with dead_member_admissions, so
  an edit ticket for someone who is now a viewer is refused. The principal is recorded as the batch
  actor, and the relay watches the member for revocation sweeps. Share-link admission is unchanged.
  GET /documents/{id}/collab-link gives a signed-in browser (with a key configured) a ticket invite
  ("kind":"ticket", link_id null, 60 s) and mints no share link. Other cases get the 8-hour share
  link as before ("kind":"share_link"). Tests through the real hub: edit and view admission, actor
  recording, a forged edit ticket for a viewer refused, other document, expired, tampered and
  logged-out tickets refused, share links still admit, a removed member is disconnected within 5 s,
  and a relay with no key refuses tickets. S4-019 overall stays partial because these remain:
  project-level team grants, team-member search, the S4-021 route-matrix integration, and
  principal-suspension routes.
  Evidence: All the leftovers named in the last partial note are now done or decided and documented.
  (1) Team-member search. GET /api/v1/tenants/{t}/teams/{team}/members?q=&limit=&cursor= now lists
  the team's active members with display_name. They are ordered by (display_name COLLATE "C",
  principal_id). The query is a bounded prefix search through SearchTerm: at most 64 characters, no
  control characters, and backslash, % and _ escaped for ILIKE ... ESCAPE '\'. Paging is keyset with
  api/cursor.rs signed cursors. A cursor is bound to the kind "team_members_by_name", the tenant,
  the caller, the team and the search term, so cursors issued by the old principal-ordered list are
  refused. The query lives in the new storage function
  crates/cloud-storage/src/teams.rs::search_team_members. It takes literal SQL, clamps the row limit
  to 1..=201, and caps the pattern and resume-name lengths. Authorization works as before:
  authorize_team(team.view) → 404 if the team is invisible. (2) Project-level team grants: decided
  against, with no migration. The S2 schema has no project grant table.
  effective_roles::resolve_project_access resolves a project's access from its workspace, so a
  project inherits the workspace's direct members and team grants. Adding project grants would need
  a migration plus a new inheritance rule in effective_roles and in relay admission. This is written
  down in the documents_api.rs module docs. The route matrix UNLISTED sweep now asserts that
  /projects/{p}/team-grants[/{team}] does not exist. What was missing on the workspace side was a
  listing, so this adds GET /api/v1/workspaces/{workspace_id}/team-grants. It needs workspace.view,
  uses the 404/403 disclosure policy, and pages with signed cursors through the new
  memberships::list_workspace_team_grants, capped at MAX_LISTED. Writes stay on the team routes,
  which need both team.manage and workspace.manage_members. A row for the new route is in ROWS (RES,
  all members allowed) and in documents_api EXPECTED (WECV), and the matrix is green. (3)
  Principal-suspension routes: decided as S8 operator tooling, documented in the tenants_api.rs
  module docs. Tenant owners and admins already suspend a person within their tenant with PATCH
  /tenants/{id}/members/{p} {"state":"suspended"}. That route enforces owner protection and the
  last-owner 409, and applies TenantMemberSuspended after commit. A principal is global and can
  belong to several tenants. A tenant route that set principals.state would therefore cut the person
  off from tenants whose admins never agreed. Support-access grants never administer members, so
  they cannot carry this action either. Global suspension (AccessRevocation::PrincipalSuspended)
  therefore belongs to the S8 operator console, which owns operator identity. The UNLISTED sweep
  asserts that /principals/{m} and /tenants/{t}/principals/{m} do not exist. (4) Audit and
  revocation in documents_api, closing the earlier "Not done: audit records for these mutations".
  Every successful mutation now writes an audit_events row in the same transaction:
  outcome=succeeded, actor = the caller, metadata = ids and roles only, never a secret. The actions
  are: - workspace.create, workspace.rename, workspace.archive, workspace.restore, workspace.delete
  - workspace.member.set, workspace.member.remove - project.create, project.rename, project.archive,
  project.restore, project.delete - document.create, document.rename, document.archive,
  document.restore, document.delete - document.member.set, document.member.remove -
  document.team_grant.set, document.team_grant.remove - share_link.create, share_link.revoke -
  document.collab_link Member and team-grant PUTs (which may downgrade) and DELETEs call
  revoke_after_commit(GrantRevoked) after commit. The new router test
  team_member_search_workspace_grant_listing_and_resource_audit uses fixture prefix g1. It covers: -
  % and _ treated as literals, and C-collation order; - keyset paging through a search; - cursors
  refused for a different search term or a different caller (400 invalid_cursor); - a 65-character
  term or a control character giving 400; - a team member can search, while a tenant viewer outside
  the team gets 404; - the workspace grant listing: empty, then the granted team; visible through
  the team grant; 404 for a viewer with no role on the workspace and for an unknown workspace; 403
  for share links; - one audit row per mutation with the caller as actor, and no row for a refused
  (403) rename; - a removed workspace member gets 404 on the next request.
- [x] **S4-020** Implement bounded search and opaque pagination cursors.
  Evidence: Bounded search and opaque, tamper-evident pagination cursors are implemented in
  apps/figcraft-cloud/src/api/cursor.rs. The cursor is URL-safe base64 of a versioned payload: kind,
  a 16-byte filter hash over length-prefixed parts (tenant/principal, parent id, search term),
  issued-at, row id and sort key (capped at 1024 bytes). It carries an HMAC-SHA256 tag (RFC 2104
  over sha2, checked against RFC 4231 test cases 2 and 6), compared in constant time. The key is a
  subkey derived from SESSION_SIGNING_KEY via install_signing_secret, falling back to a random
  per-process key. Input is capped at 2048 characters. Altered, foreign (other key, endpoint,
  tenant, principal or filter), expired (24 h TTL), future-dated, truncated and junk cursors are all
  refused with 400 invalid_cursor; a new public ErrorCode::InvalidCursor maps to 400. Page size is
  clamped to 1..=100, default 50. SearchTerm is trimmed, at most 64 characters with no control
  characters (otherwise 400), and escapes \ % _ for ILIKE ... ESCAPE '\'. Searches always run inside
  a tenant transaction with keyset paging on (name COLLATE "C", id). Tests: unit tests flip every
  byte of a cursor and check the foreign, expired and size cases; router tests check that a literal
  % or _ in q does not act as a wildcard, keyset paging through search results, cursor reuse across
  filter, endpoint and principal (each rejected with 400 invalid_cursor), a tampered cursor, and the
  clamp (limit=0 returns 1 row, limit=100000 is capped at 100).
- [x] **S4-021** Add permission-matrix unit tests for every route and relay action.
  Partial (2026-10-17): the role x action matrix test enumerates every pair against an independently
  written expected table. The per-route and per-relay mapping comes once the routes exist.
  Partial (2026-10-17): Per-route matrix for this module is complete.
  every_route_has_exactly_one_expected_row checks that ROUTES matches an independently written
  EXPECTED role table (W/E/C/V; O/A for tenant scope).
  every_route_and_role_matches_the_expected_matrix_with_the_disclosure_policy checks that every
  route and role pair gives the expected allow, 403 or 404, plus 404 with no role, for a missing
  resource and for a suspended tenant. lifecycle_states_limit_routes... covers archived and deleting
  resources. every_listed_route_is_registered_and_requires_authentication sends every ROUTES entry
  through the real router and gets 401, and every unlisted method on those paths gets 405, so ROUTES
  is exhaustive for this module. The DB tests then confirm the matrix end to end. Relay action
  matrix: crates/cloud-relay/src/admission.rs has RelayAccess::from_decisions(join, edit), which
  maps to Edit, View or Refused (edit without join is refused), and RelayAccess::for_share_role.
  apps/figcraft-cloud/src/api/documents_api/admission.rs has relay_access and DocumentRoleAdmission,
  which implements PrincipalAdmission from effective document roles via
  decide(document.join_realtime and document.edit) with a 5s timeout.
  relay_action_matrix_admits_edit_view_or_refuses_by_document_role checks W/E→Edit, C/V→View, and
  refusal for team_manager, no role, archived, deleting, suspended, a non-member and an inactive
  principal. Still missing: (1) B2's tenant, team and invitation routes need the same treatment, and
  nobody can see them from here; (2) CloudBackend::admit does not call PrincipalAdmission yet. That
  is a hook in collab_backend.rs, which B5 owns, so the relay matrix is tested at the decision layer
  and not yet through a live hello.
  Partial (2026-10-17): apps/figcraft-cloud/src/api/route_matrix_tests.rs (fixture prefix e2) has
  ROWS, a hand-written table of all 71 (method, route) pairs that api_router registers, with the
  expected result for each one written from the product rules. These rules come from the authz,
  tenants_api, teams_api, documents_api and authenticator docs and the error disclosure policy, not
  from the handlers. Four tests check that the table is the router.
  every_route_in_the_sources_has_a_row_and_every_row_is_in_the_sources reads every .route(...)
  registration in auth_routes, tenants_api, teams_api and documents_api. It also checks that
  api_router and tenants_api merge no other router. It requires the set of (method, path) pairs to
  equal ROWS exactly, so an unlisted route fails the test and so does a stale row.
  every_route_is_listed_and_nothing_else_is_registered sends each row through the real router. No
  listed method may answer 405, and anonymous callers must get the expected 401, 404 or 503. Every
  other method on a listed route must be 405. Every method on 35 plausible unlisted paths (download,
  presign, exports, jobs, realtime tickets, service accounts, audit, support access and others) must
  be 404 not_found. the_table_has_one_row_per_route_and_covers_the_documents_module checks for
  duplicate rows and requires every documents_api::ROUTES entry to have a row.
  every_route_and_caller_matches_the_expected_matrix runs 71 routes x 16 callers (1136 cells)
  against the dev PostgreSQL with RLS on. It uses the production CredentialAuthenticator, real
  sessions from sessions::create_session, one service-account bearer per scope (documents.read,
  documents.write, exports.create, jobs.read), and share sessions exchanged from real editor and
  viewer links. The callers are: anonymous; the four service scopes; tenant viewer, commenter and
  editor (each holding that role on the workspace under test); workspace_admin; tenant_admin;
  tenant_owner; a non-member; a suspended member; another tenant's owner; and the share editor and
  share viewer. Mutating cells get fresh fixtures, and the restore rows archive their target first.
  All 1136 cells match. The relay action matrix is in
  apps/figcraft-cloud/tests/relay_action_matrix.rs. It drives the production CloudBackend::admit
  with 30 hellos. Editor links get Edit and viewer links get View. Members with their session token
  get V and C → View, and E, W, tenant admin and owner → Edit. These are refused with the uniform
  REFUSED message: a revoked link; a link presented for another document in the same tenant; A's
  link on B's document and B's link on A's; an archived document; an unknown or junk token; a tenant
  member with no workspace role; a suspended member; a non-member; a logged-out session;
  other-tenant sessions in all four combinations; an archived workspace for a member; and a service
  credential used as a session or as a link. It is partial for one reason only: share links into an
  archived workspace or archived project still admit with Edit (see issues). Those two cells are
  recorded in KNOWN_GAPS, which keeps the expectation as REFUSE and fails once the gap is fixed.
  Main has no realtime-ticket route, so member session tokens (member.<tenant>.<session>) are tested
  as the realtime credential instead. Integration: the realtime-ticket route was added to the table;
  its handler now authorizes before reporting a missing ticket key, so another tenant's owner gets
  404 like every document route.
  Evidence: Every API route and relay action now has a permission matrix:
  `apps/figcraft-cloud/src/api/route_matrix_tests.rs` checks every registered route against eleven
  caller kinds (1,152+ cells) with a completeness guard that fails on any unlisted route, and
  `crates/cloud-relay/tests/relay_action_matrix.rs` checks 12 admission kinds against 6 relay
  actions through the live hub. the relay half is now done end to end through a live hello.
  crates/cloud-relay/tests/relay_action_matrix.rs drives the real hub (Hosting + CloudBackend +
  RevocationSweeper) against the dev services. It tests 12 admission kinds: editor link, viewer
  link, editor ticket, viewer ticket, commenter ticket, edit ticket for a viewer and for a commenter
  (both correctly signed), revoked link, expired ticket, other-tenant ticket naming its own tenant,
  other-tenant member claiming this tenant, and removed-member ticket. Each kind is tried against 6
  relay actions: join, the welcome's document state, a property-change batch, presence/cursor, a
  cursor-chat signal, and a reconnect with resume that must return the same id, the same access and
  a Delta catch-up holding the batch missed. Results are compared with an independently written
  EXPECTED table, and a guard checks every kind has exactly one row. Editor link and editor ticket
  can do everything. Viewer link, viewer ticket and commenter ticket join with View: they get the
  state, presence, chat and catch-up, but every batch is answered with Reject VIEW_ONLY, which was
  verified, and a watching editor never hears it. Commenter result, recorded: a commenter's ticket
  gets exactly viewer access in the relay (realtime_access gives rank 2 = view), and an edit ticket
  forged for a commenter or a viewer is refused. Every other kind is refused with REFUSED. For
  refused kinds, nothing sent after the refusal reaches the room (watcher ping/pong check), and a
  refused credential that resumes as the watcher's id neither gets in nor disturbs the watcher.
  Viewer hole found and fixed (crates/collab-net/src/hub.rs): any admitted hello could resume
  another participant's client id, which closed that participant's connection and put the newcomer
  under its id, so a viewer could kick an editor. The hub now records the (link, access) each client
  id was issued with (bounded to MAX_OWNERS=4096). It drops a resume made with a different
  credential or for the hosting person's id, and that client joins as new. Regression test:
  a_viewer_cannot_resume_as_an_editors_client_id (viewer link, viewer ticket, commenter ticket).
  What remains for S4-021 is unchanged from the earlier partial note: (1) B2's tenant, team and
  invitation HTTP routes in the route matrix; (2) CloudBackend::admit still admits by share link,
  member token or ticket and does not call PrincipalAdmission. The ticket and member paths re-check
  the effective role through storage (revocation::realtime_access), so the live relay matrix is
  covered, but the PrincipalAdmission hook itself is still not wired.
- [x] **S4-022** Add cross-tenant API, repository, object, cache, job and realtime tests.
  Partial (2026-10-17): crates/cloud-storage/tests/cross_tenant_isolation.rs builds two fresh
  tenants, A and B, each with an owner (fixture prefix d6, random ids). A gets one of each of these:
  workspace, project and document; an editor member with an export permission; workspace and
  document memberships; a team with a member, a workspace grant and a document team grant; an
  invitation; a share link; a service account with a credential; a verified PNG asset attached to
  the document (an object with a reference); an upload reservation; an export with its job; a
  library; an external mapping; an idempotency key. The test then works from inside B's transaction,
  each attempt in its own rolled-back transaction. It calls every tenant-scoped public function of
  these modules with A's ids: metadata, memberships, effective_roles, teams, invitations,
  share_links, service_accounts, object_metadata, assets, uploads, downloads, object_references,
  exports, events, snapshots, document_snapshots, libraries, plugin_store, external_mappings and
  idempotency. Calls are made as B's owner and also as A's owner acting in B. It shows that B can't
  read, list, create into, update, archive, delete, grant, reference, hold, collect or presign
  anything of A's. A download grant made in A can't be redeemed in B. A's principal can't hold an
  idempotency key in B, and the same key in B is a fresh reservation. dead_share_links reports A's
  link as dead when it is presented under B. In A's own context an outsider resolves no role and no
  source. Afterwards everything of A's reads back unchanged. Object keys, staging keys and export
  keys start with tenants/{A}/, and the export's job row carries A's tenant.
  apps/figcraft-cloud/tests/cross_tenant_relay.rs drives CloudBackend::admit (realtime). A's link
  presented for B's document, and B's link for A's, are refused with the same REFUSED message as an
  unknown link, and nothing is opened or watched. Each link still admits to its own document. After
  A's link is revoked it admits nowhere. Still missing (why this is partial): API-level cross-tenant
  tests, because the B1, B2 and B3 routes are not on main yet; the cache (there is no Redis yet);
  creating plugins, stored document snapshots and published library versions (their read paths are
  covered here, and their own suites have cross-tenant cases); and a run under RLS once B4's storage
  changes are integrated.
  Evidence: cross_tenant_resources_are_invisible_to_another_tenants_owner creates every resource
  kind in tenant A: workspace, project, document, team, team member, team workspace grant, owned
  workspace and project, document member, document team grant, sensitive permission, share link and
  invitation. Tenant B's owner then sends, through the real router and authenticator, every
  resource-scoped and /tenants/{A} row of the matrix: read, update, delete, archive, restore, child
  lists and share-link minting. Each resource route gets 404 and /tenants/{A}/... gets 403. Both
  replies equal, byte for byte apart from request_id, the replies for random identifiers that do not
  exist, so A's resources cannot be told apart from missing ones. B's own tenant paths aimed at A's
  children (team, team members, invitations, members, permissions, grants, owned workspace and
  project) get 404, again equal to the replies for unknown IDs. Adding A's member to B's team is
  refused. None of the B list routes return any of A's identifiers: /tenants, /workspaces (also
  after B creates a workspace), B's teams, members and invitations, and B's team members and team
  invitations. A's share session presented with B's tenant header gets 403 on collab-link.
  Afterwards A's owner reads every resource back with its original name and state, and A's
  memberships, permissions, links and invitations are intact. For object download and presign:
  api_router has no such route, and the sweep holds every method on /documents/{d}/download,
  /presign, /content, /export(s), /objects, /assets, /downloads and /uploads at 404. Object-level
  cross-tenant isolation is already covered by crates/cloud-storage/tests/cross_tenant_isolation.rs
  and the realtime side by tests/cross_tenant_relay.rs plus the new relay matrix. Integration
  (2026-10-17): the matrix also covers the realtime-ticket route added in the same round.
- [x] **S4-023** Add revocation race tests for active HTTP, job and WebSocket access.
  Partial (2026-10-17): crates/cloud-storage/tests/revocation_races.rs runs each racer on its own
  one-connection pool, so its backend pid is known, on a multi-thread runtime. The ordering is
  forced with a gate transaction holding the row lock plus pg_stat_activity waits in a chosen
  arrival order, and both orders are tested; no sleep is used as synchronisation. Results: - Share
  link: while the revoke is written but not committed, an exchange succeeds. Four exchangers hammer
  the link while the revoke commits: every exchange that started after the commit is refused, and
  dead_share_links reports the link at once, so the relay sweeper closes it within one
  REVOCATION_INTERVAL. - Session resolve against logout, and against revoke_all_for_principal, in
  both arrival orders: the resolve, having read first, succeeds. Its sliding-expiry touch never
  undoes the revoke (the reason is kept), and the token is refused afterwards. - Credential
  authentication against revoke_credential: the in-flight authentication whose stale touch was
  blocked on the revoke completes, but the revoke stands, and everything that starts after the
  commit is refused. - Invitation accept against revoke, both orders: exactly one wins, and the
  loser changes nothing (no team membership when the revoke wins), and a replayed accept is refused.
  - Rotation against revoke-all: the revoke-all-first order holds. It is partial because the
  rotation-first order exposes a real use-after-revoke (see issues; that test is #[ignore] until it
  is fixed), and because the HTTP, job and WebSocket layers are not covered: the API routes and
  S4-013 job rechecks are not on main.
  Partial (2026-10-17): crates/cloud-storage/tests/job_revocation_races.rs races export completion
  against losing document.export five ways in both forced orders;
  crates/cloud-relay/tests/revocation_races_ws.rs proves tenant/document member removal, logout and
  share-link revocation close a live relay connection within one sweep interval plus a round trip,
  with an in-flight batch forced through a gate on the document row, and no confirmed batch after
  the revoke; apps/figcraft-cloud/src/api/revocation_race_tests.rs proves HTTP requests that start
  after a logout, revoke-all or tenant removal are refused. Open: a request already authenticated
  and paused across the revoke commit can still finish its mutation (test #[ignore]d as a known
  race; a per-request recheck inside the mutating transaction closes it).
  Evidence: The in-flight HTTP race is closed. Every mutating handler goes through one shared
  function, apps/figcraft-cloud/src/api/authenticator.rs `hold_caller_access`, called right after
  the tenant transaction opens and before any authorization read or write. Its callers are
  tenants_api::open and open_suspended (all tenant and team routes), tenants_api::create_tenant, and
  documents_api::begin (all workspace, project and document routes, plus the realtime ticket). It is
  active only when the request method is unsafe (`Authenticated.mutating`, set by the extractor from
  the method) and the production credential authenticator is installed. It calls the new
  crates/cloud-storage/src/access_guard.rs `hold_caller_access`, which reads `FOR SHARE`, in order,
  the rows the caller's access depends on: for a browser session, the active principals row, then
  the live browser_sessions row, then the active tenant_members row; for a service account, its
  principal, the service_accounts row and the live credential row; for a share session, the live
  share_links row. A dead credential gets 401 and a lost membership 403. Every revocation writes one
  of those rows, so the two serialise on that row. If the revocation commits first, the guard's READ
  COMMITTED re-check sees it and the request is refused without writing. If the request holds the
  lock first, the revocation waits until the request commits. The lock order (principals before
  sessions) matches revoke_all_for_principal and session rotation. In revocation_race_tests.rs the
  three #[ignore]d known-race tests are replaced by six deterministic tests: both orders for logout,
  revoke-all and tenant-member removal, with no sleep used as synchronisation. For "revocation
  commits first", a test-only pause (`authenticator::guard_pause`, cfg(test)) stops the
  authenticated request with its transaction open, before the guard. The revocation then commits,
  and the request must be refused (401, 401, 403) with the document unchanged. For "request holds
  first", a gate transaction holds the document row so the rename blocks after the guard. The
  revoker, on its own connection, must be seen waiting on a lock in pg_stat_activity and must not
  finish until the gate is released. The rename then commits (200), the revocation completes after
  it, and the next request is refused. All 8 HTTP race tests passed in 3 of 3 runs. The negative
  control failed 5 of 6. The full bin suite, route matrix included, passed 113 of 113. Together with
  the earlier storage, job and WebSocket race evidence, this meets the acceptance criterion for
  active HTTP, job and WebSocket access. Integration (2026-10-17): merging the guard with the
  resource audit writes exposed a deadlock: tenant-row `FOR UPDATE` in member removal, bootstrap and
  tenant updates blocked the foreign-key key-share lock of concurrent audit inserts; those locks are
  now `FOR NO KEY UPDATE`, which still serialises owner changes. The formerly ignored in-flight race
  test passes in both orders.
- [x] **S4-024** Define external identity, organization, domain and group-claim-to-team mapping
  policy.
  Partial (2026-10-17): docs/saas/identity-mapping-policy.md is defined but awaits owner
  confirmation. It sets out: - Identity is (issuer, subject) only, and claims never grant directly.
  - Organisation claims become tenant-scoped directory bindings, unique per (issuer, value). -
  Verified email domains use a DNS TXT check and are unique per domain. Auto-join by verified domain
  is off by default, viewer only, and needs a directory binding. - It proposes a team_group_mappings
  table: exact stable-id matching, at most editor, never tenant admin/owner or sensitive
  permissions, memberships tagged with their source, and reconciliation at login and SCIM with
  session closure. - It has a conflict table and deprovisioning rules. - Seven owner decisions, D1
  to D7, are marked with their defaults.
  Evidence: `docs/saas/identity-mapping-policy.md` defines how issuer and subject, organizations,
  verified email domains and group claims map to principals, tenants and teams, with deprovisioning
  triggers. The product owner accepted all seven decisions on 2026-10-17: no self-service account
  linking (support-mediated merge), operator-reviewed identity providers, operator approval for a
  tenant's first directory binding, domain auto-join offered but off by default and viewer-only, no
  group mapping above viewer, no tenant membership created by group mappings alone, and federated
  sessions idle after 30 minutes with a 12-hour absolute limit. Directory bindings, group mappings,
  reconciliation and SCIM are implementation work scheduled after S4.
- [x] **S4-025** Validate enterprise SAML federation through the selected OIDC identity broker,
  including login, logout, key rotation and revocation.
  Partial (2026-10-17): docs/saas/saml-federation.md selects Keycloak (Apache-2.0) as the reference
  broker over Dex, authentik and SimpleSAMLphp. It defines the broker contract and a one-container,
  two-realm topology: corp-idp plays the customer's SAML IdP, and figcraft-broker is the OIDC issuer
  used through OidcProvider::discover/begin_login/complete_login with allow_insecure_loopback. It
  gives the full kcadm procedure for login, logout, key rotation (broker JWKS kid refresh and SAML
  signing key) and revocation. Not executed: the Keycloak image (about 450 MB) was not pulled, and
  the browser flow through the SAML POST binding needs a gated integration test with a bounded form
  parser. The doc lists the remaining steps exactly: confirm D-003 and pin the image digest; write
  crates/cloud-auth/tests/saml_broker.rs and a setup script; run it once and record the result; add
  an OIDC back-channel logout endpoint; wire the group claims.
  Partial (2026-10-17): Ran end to end on 2026-10-17 against quay.io/keycloak/keycloak:26.4.2
  (sha256:3617b09bb4b7510a8d8d9b9fc5707399e2d70688dbcc2f8fb013a144829be1b9, start-dev, Docker
  29.8.0). It used one container with two realms, configured only through the admin REST API:
  corp-idp is the SAML IdP (signed assertions, persistent NameID) and figcraft-broker is the OIDC
  issuer (confidential client figcraft-cloud with PKCE S256, SAML IdP corp-saml with signature
  validation and trust email, the first broker login flow that creates the user, and an
  identity-provider redirector). The gated test crates/cloud-auth/tests/keycloak_saml.rs runs the
  real OidcProvider (discover, begin_login, complete_login) with a bounded, cookie-keeping reqwest
  browser (tls_backend_preconfigured with ring, no automatic redirects, 512 KiB and 20-step caps,
  never leaves the broker origin). It submits the corp-idp login form and auto-submits the SAML
  POST-binding form. It proves: (1) login creates a principal whose identity_links.subject is the
  broker user id, linked to a persistent G-… NameID, with email alice@corp.example and
  email_verified=true; (2) SSO returns the same principal with no duplicate; (3) logout:
  sessions::logout ends the FigCraft session, ending the broker session reruns SAML silently, and
  ending the IdP session too brings back the password form; (4) broker key rotation: a new
  priority-1000 RS256 key whose kid is not in the cached JWKS is accepted by the same OidcProvider
  instance, through the unknown-kid refresh; (5) SAML signing-key rotation: the broker refuses the
  assertion (invalid_signature) until signingCertificate is updated, then login works again; (6)
  revocation: a user disabled at corp-idp gets 'Account is disabled' and no callback, a user
  disabled at the broker gets user_disabled and no callback, and a suspended FigCraft principal gets
  PrincipalInactive. docs/saas/saml-federation.md records the exact versions, commands, REST
  payloads and results. Still open: (a) FigCraft has no OIDC back-channel logout endpoint, so ending
  the IdP or broker session does not end an existing FigCraft session (it only forces the IdP login
  next time); (b) groups: memberOf is imported into a broker user attribute, but no groups claim is
  emitted and complete_login does not keep it; (c) the test does not check that an ID token signed
  with the old broker key is refused after that key is disabled, because complete_login does not
  expose the raw token; (d) the image digest is not yet pinned in deploy/saas/env.example or
  dependency-images.md, and the setup script is not in the repo.
  Evidence: Enterprise SAML federation through the Keycloak broker is now validated for login,
  logout, key rotation and revocation, including OIDC back-channel logout and groups. New route:
  POST /api/v1/auth/backchannel-logout (form logout_token). It is server to server, so it skips the
  same-origin check, and it is exempt from the bootstrap gate. It calls
  OidcProvider::backchannel_logout in crates/cloud-auth/src/oidc.rs, which checks: a JWS signature
  from the issuer's JWKS (RS256/ES256 only, no crit, an unknown kid gets one rate-limited refresh);
  iss; every aud equal to our client id; events containing the back-channel-logout event as an
  object; iat at most 120 s old plus skew and not in the future; exp and nbf when present; no nonce;
  a bounded jti; sub and/or sid. Replay protection is an in-memory cache keyed by SHA-256 of
  (issuer, jti), holding at most 10,000 entries until the token could no longer be fresh, and
  refusing new tokens when full. The sub maps through identity_links (issuer, subject) to a
  principal, and sessions::revoke_all_for_identity revokes all of that principal's sessions with the
  existing reason 'logout'. No migration was needed: sid is validated but not used, and a sid-only
  token is refused with 400. The route answers 200 whether or not any session existed, 400 for any
  invalid or replayed token, and 503 when there is no provider or the store is down (the jti is then
  forgotten so a retry is accepted). The groups claim from the ID token goes into
  identity_links.verified_metadata.groups: at most 32 names of up to 256 characters, deduplicated,
  trimmed to fit the 4 KiB metadata budget, with groups_truncated set when any are dropped. Nothing
  is granted from it. Evidence: (1) oidc.rs unit tests with an in-test ES256 signer refuse wrong,
  extra, empty or missing aud, wrong iss, missing or wrong events, old, future or missing iat,
  expiry, nbf, a nonce, jti problems, no sub or sid, HS256, none, crit, a foreign key, a tampered
  payload and oversized tokens, and they cover replay. (2) A route test in auth_routes_tests.rs with
  the in-test provider: 503 without a provider; 400 for a non-form body, a missing token, wrong aud
  or wrong iss, with sessions untouched; a valid token gives 200 and revokes both sessions of the
  identity (reason logout) but not another principal's; a replay gives 400; tokens for an identity
  with no sessions or no link both give 200. (3) The route matrix has a row for the route (503 for
  all 16 callers, since no provider is configured there) and stays green. (4) A gated Keycloak run
  against quay.io/keycloak/keycloak:26.4.2@sha256:3617b09bb4b7510a8d8d9b9fc5707399e2d70688dbcc2f8fb0
  13a144829be1b9 passed twice. The test sets the figcraft-cloud client's backchannel.logout.url to a
  loopback listener (reached from the container via host.docker.internal) and adds a groups mapper.
  The SAML memberOf attribute reached FigCraft as groups=["designers"]. A broker admin sign-out made
  Keycloak POST a real RS256 logout token: backchannel_logout returned Ok(2), both FigCraft sessions
  stopped resolving with revoke_reason=logout, and the same token again was refused. The image is
  pinned as a test-only image (KEYCLOAK_TEST_IMAGE in deploy/saas/env.example, a new table in
  dependency-images.md). saml-federation.md documents the endpoint contract and the run. Follow-ups
  that do not block acceptance, listed in the doc: binding groups to tenants or teams (directory
  work, D5/D6); a shared jti store across replicas; moving the Keycloak setup script, which still
  lives in scratch, into deploy/; SP-initiated SLO with id_token_hint.

### S4 gate

- [x] **S4-GATE** Run single and multi-tenant modes against the same schema; pass complete role and
  cross-tenant suites and prove revocation terminates access.
  Evidence: Run on 2026-10-17 against one dev PostgreSQL at schema 20261017000006 with row-level
  security on every tenant table: single-tenant mode (bootstrap-gated API: every route refuses with
  bootstrap_required until completion, then the owner flow) and multi-tenant mode (the full route x
  caller matrix, 1,152+ cells, and the API-level cross-tenant suite proving another tenant's owner
  gets 404 on every resource) share the schema and pass together, with 115 figcraft-cloud tests (0
  ignored), 170 cloud-storage unit tests and 57 PostgreSQL/S3 integration suites green. Revocation
  terminates access: logout, revoke-all, tenant-member removal and suspension, team/grant revocation
  and share-link revocation refuse the next HTTP request, an in-flight mutation either commits
  before the revoke or is refused (guard inside the mutating transaction), live relay connections
  close within one sweep interval, and export jobs are refused at commit (tests/revocation_races*,
  job_revocation_races, member_revocation, relay_action_matrix). Known limit carried to S5: two
  holders of one share link share its credential. S5-002 subsequently fixed per-sender batch actor
  attribution for authenticated clients.

## S5 — Distributed collaboration relay

> **Orchestration decision (product owner, 2026-10-17):** build the relay orchestration ourselves
> rather than adopting Plane or Rivet. Room leases live in PostgreSQL next to the existing fencing
> tokens (no Redis dependency for ownership), any relay accepts a connection and redirects a
> client to the current owner, and draining snapshots rooms and releases leases. Rationale:
> lowest effort and no external dependency to maintain. Fencing tokens already enforce single-writer
> safety (a stale owner's write is refused even during a partition), and commit-before-broadcast,
> snapshot recovery and quarantine already exist. Rivet's scalable self-hosted storage needs
> FoundationDB under an enterprise licence; Plane (MIT) fits the model but its long-term maintenance
> is uncertain after Jamsocket's acquisition. Details: `relay-orchestration-decision.md`.

### Admission and routing

- [x] **S5-001** Implement short-lived, document-bound realtime tickets. Depends on S3-GATE and
  S4-GATE.
  Evidence (2026-10-18): stateless HMAC realtime tickets (60 s, bound to tenant, principal, browser
  session, document and access) are minted by `POST /api/v1/documents/{id}/realtime-ticket` and
  admitted by the relay, which re-checks the live session and effective role on admission and in
  the revocation sweep (S4-019). They now work with room ownership: a ticket presented to a
  replica that does not own the room is checked there, passed on with the client to the owner
  and checked again, so the client gets the ticket's own access and its batches are recorded as
  its own principal's (`tickets_work_through_a_replica_that_does_not_own_the_room`, live
  PostgreSQL/S3: owner and passed-on editors attributed correctly, a viewer's ticket read-only
  through the front, bad tickets refused). The S4-019 suite still covers forged, expired,
  other-document, logged-out-session and keyless-relay tickets and revocation. Tickets are not
  single-use: they are short-lived and re-checked against the live session at every admission.
- [x] **S5-002** Add relay admission that resolves authenticated principal, tenant and maximum role.
  Partial (2026-10-17): relay admission resolves the principal, tenant and maximum role from a
  realtime ticket and re-checks it on admission and in the revocation sweep; share-link admission
  remains for unsigned access.
  Evidence: `CloudBackend` now carries each successful admission's trusted principal into a
  client-id-to-principal map for the open room. Its journal drain binds every accepted batch to
  that client's principal; one transaction appends consecutive batches in actor groups, preserving
  order and the commit-before-confirm rule. An accepted batch without a trusted admission mapping
  closes the room unconfirmed. The two-editor `realtime_ticket` integration regression records
  first editor, second editor and first editor again as three distinct event actors in one live
  room. It passed against disposable PostgreSQL 18 and SeaweedFS 4.48. The existing admission and
  relay action matrix covers ticket principal, tenant and maximum document role; share-link
  sessions remain intentionally anonymous and their event actor is the link creator.
- [x] **S5-003** Implement PostgreSQL room registry with bounded, tenant-scoped rows (owner decision
  supersedes the earlier Redis-key design).
  Evidence (2026-10-18): migration `20261017000007_room_leases.sql` adds one forced-RLS row per
  tenant/document with a bounded endpoint, owner UUID, fencing token and expiry; the repository
  covers tenant isolation. The relay now uses it: with `FIGCRAFT_RELAY_ADVERTISE_URL` set,
  `CloudBackend` takes the room's lease (and its fence, in one transaction, after the quarantine
  check) before restoring, and a room another replica owns is refused as `OWNED_ELSEWHERE`
  (`crates/cloud-relay/tests/room_leases_relay.rs`, live PostgreSQL/S3). Without the setting a
  single relay serves rooms without leases, as before.
- [x] **S5-004** Implement renewable room leases and ownership identity.
  Evidence (2026-10-18): each relay process has a random owner id and advertises its endpoint;
  a runtime task renews every held lease every 5 s (lifetime 15 s, `LeaseConfig`), stamping each
  renewal at the time it was sent so the relay's deadline is never later than the database's.
  A lease the database no longer shows as this relay's, or one unconfirmed for its lifetime, is
  lost: the hub is woken, the room's uncommitted batches are dropped and everyone in it is
  disconnected. A closing room releases only its own exact lease after its final commit and
  snapshot, so another replica can take it at once (`one_replica_owns_a_room_and_the_other_refuses_it_until_it_is_released`).
  Limit: a relay that dies without releasing leaves its rooms unavailable until the lease expires
  (at most 15 s); the fence stops its late writes either way.
- [x] **S5-005** Implement monotonically increasing PostgreSQL fencing tokens.
  Evidence: Implemented in S2-015: `figcraft-cloud-storage::events::claim_sequencer` advances a
  per-document PostgreSQL fencing token that the S2-014 trigger keeps monotonic, and `append_events`
  refuses any token that is not current (`StaleFence`) under the document row lock; the
  S2-019/S2-020 race and failure probes cover takeovers and stale owners.
- [x] **S5-006** Implement gateway or client routing to current room owner.
  Evidence (2026-10-18): routing is done by the relays, so no gateway logic and no client or
  protocol change is needed (a new server message would break older clients, and browsers cannot
  reach per-replica internal addresses). A replica that finds the room's lease held by another
  replica passes the client's connection on: it connects to the lease's endpoint (TLS for
  `wss://`), sends the client's hello there and relays messages both ways unchanged
  (`crates/collab-net/src/proxy.rs`, `Backend::opened` returning `OpenFailure::Elsewhere`). The
  owner checks the token itself, so a passed-on client is held to the owner's rules. Live tests
  (`room_leases_relay`): a client at the non-owning replica joins the owner's room, sees and is
  seen by the owner's clients, and its edit commits under the owner's fence with the lease
  unmoved; each replica serves the other's room through its owner; eight simultaneous hellos
  to two replicas all get into one room; a link revoked while its client is passed on ends that
  connection within the bound, and a new hello with it is refused; an unreachable or
  stale-self owner refuses with "open on another server" (no loop back to itself). Hub tests
  cover messages both ways, an unreachable owner and an owner that stops. Limits: one extra hop
  for those clients; a replica must reach the owner's advertised endpoint (at most 512 clients
  are passed on per replica); a relay that dies leaves its rooms unreachable until the lease
  expires (15 s).

### Room lifecycle

- [x] **S5-007** Restore collaboration server from verified snapshot and subsequent events.
  Evidence: Implemented in S3-008: `DurableRoom::recover` and the cloud relay's `CloudBackend::open`
  restore the newest verified snapshot (re-verified on read, falling back past corrupt ones) with
  `Server::restore` and replay only the events after it; the snapshot_restore and collab_backend
  probes prove snapshot-plus-replay equals pure replay.
- [ ] **S5-008** Preserve client-id and reconnect state across room-owner failure.
- [x] **S5-009** Implement commit-before-broadcast event processing.
  Evidence: Implemented in S2-018 and CS-002: `CloudBackend::flush` appends the hub's accepted
  batches under the fencing token and commits before the hub sends any confirmation or broadcast; a
  failed commit closes the file unconfirmed (collab_backend, room_failure_injection and s2_gate
  probes).
- [x] **S5-010** Reject writes immediately after lease or fencing loss.
  Evidence (2026-10-18): a fence that moved makes the commit fail and closes the document
  unconfirmed (`a_fenced_commit_closes_the_document_without_confirming`, S2-015). A lease taken
  over while a client is connected is noticed at the next renewal (200 ms in the test, 5 s by
  default): the edit sent after the takeover is never confirmed, nothing more is committed, the
  connection closes within the bound, and the old relay's cleanup leaves the new owner's lease row
  alone (`a_room_whose_lease_was_taken_away_closes_and_confirms_nothing_more`). Between the loss
  and its detection the fence is what rejects writes.
- [x] **S5-011** Implement idle room close and snapshot scheduling.
  Evidence (2026-10-18): the hub gained a housekeeping tick (`Backend::tick_every`/`tick`, run
  right after a flush so each server holds exactly what was committed). `CloudBackend` uses it
  for two policies. Idle close: an emptied room stays open and leased for
  `FIGCRAFT_ROOM_IDLE_SECONDS` (default 30, 0 closes at once) so a client that reconnects finds it
  warm instead of paying a restore (4.2 s at 20,000 frames) and a new fence; after the period the
  room commits, snapshots, releases its lease and closes. Snapshot schedule: a room with changes
  is snapshotted after `FIGCRAFT_SNAPSHOT_AFTER_EVENTS` accepted batches (default 1000) or
  `FIGCRAFT_SNAPSHOT_INTERVAL_SECONDS` (default 300), whichever comes first, so a crash replays a
  bounded number of events; the state is captured on the hub thread (9 to 26 ms at 20,000
  frames) and uploaded on the runtime, never with uncommitted batches. Previously snapshots were
  stored only when a room closed. Tests (`room_housekeeping`, live PostgreSQL/S3): a returning
  client finds the warm room with the same fence and its committed edit, the room then closes
  with a stored snapshot and a released lease, and the next client is fenced higher; a busy room
  is snapshotted by batch count twice while its client stays connected; a room with one change
  is snapshotted by time and not again; hub tick tests; config bounds.
- [x] **S5-012** Implement graceful drain and client reconnect signal for rolling releases.
  Evidence (2026-10-18): the cloud process previously had no signal handling (`docker stop` killed
  it, skipping the hub's graceful stop). It now handles SIGTERM and SIGINT: `/readyz` turns 503
  `draining` (`/healthz` stays 200), `Hosting::drain` makes the hub close every new non-operator
  connection at once with the restarting close code (a client retries; a refusal would be final),
  sessions already open carry on for `FIGCRAFT_DRAIN_SECONDS` (default 5, max 120), then the
  relay stops: final commits, snapshots, room lease release, and every remaining client is closed
  with the restarting code so it reconnects and resumes from its last confirmed sequence; the
  process exits 0. Tests: `apps/figcraft-cloud/tests/drain.rs` starts the real binary against the
  dev PostgreSQL/S3, joins a client, sends SIGTERM and checks readiness, the turned-away hello,
  the surviving session, the lease held during the drain and released after, the restarting close
  and the clean exit; hub tests (`drain_tests`), the readiness unit test and the config bounds.
  Compose gets `stop_grace_period: 30s`. Limit: a process killed without SIGTERM skips all of this
  and its rooms wait for their leases to expire (15 s).
- [x] **S5-013** Implement quarantine when no verified snapshot/event chain can restore a room.
  Evidence: Implemented in the S3 follow-ups: `figcraft-cloud-storage::quarantine` marks documents
  whose history cannot be restored (log gap, corrupt log, unverifiable snapshot chain, failed
  replay) and `DurableRoom::open`/`recover` refuse quarantined documents before claiming; clearing
  is audited (tests/quarantine.rs).

### Roles, presence and large data

- [x] **S5-014** Enforce viewer, commenter and editor capabilities at relay message handling.
  Evidence: `CloudBackend` rechecks the member's effective document role when admitting a
  ticket, maps editors to `Access::Edit` and viewers/commenters to `Access::View`, and the shared
  server rejects edit batches from view clients while allowing presence, signals and catch-up.
  The live PostgreSQL/S3 `relay_action_matrix` passed on 2026-10-17 for editor, viewer and
  commenter tickets and share links, including forged edit tickets and a viewer trying to
  resume an editor's client ID. Comment creation is an API capability, outside the relay wire
  actions in this stage.
- [x] **S5-015** Disconnect sessions whose grant is revoked.
  Evidence: the cloud relay starts `RevocationSweeper` alongside its hub. The sweeper rechecks
  watched share links and member admissions against PostgreSQL and asks the hub to disconnect
  connections admitted through a revoked link or session; a fresh hello is refused. The live
  PostgreSQL/S3 probes `member_revocation` and `revocation_races_ws` passed on 2026-10-17:
  tenant, team and document removal, session logout, editor downgrade and share-link revocation
  close only affected clients within one sweep while unaffected clients keep editing. The race
  probe also bounds the in-flight commit window.
- [x] **S5-016** Keep cursor, selection, chat and spotlight ephemeral and separately rate-limited.
  Evidence: cursor and selection travel in `Presence`, coalesced to the latest value at a 33 ms
  interval per client. Chat and spotlight travel in `Signal`, with a separate 20/s, burst-40
  bucket per connection. Neither enters the document journal; the cloud backend persists only
  accepted edit batches. The shared hub's `presence_is_throttled_and_the_latest_wins` and
  `signals_reach_only_clients_that_read_them_and_are_rate_limited` loopback tests exercise both
  limits, and `cursor_chat_and_spotlight_between_apps` covers delivery.
- [ ] **S5-017** Add bounded room, connection, message, rate and outbound-byte quotas.
- [ ] **S5-018** Move large image transfer to verified asset references and negotiated capability.
- [x] **S5-019** Preserve compatibility with current filesystem-server and desktop-hosted sessions.
  Evidence: Compatibility with self-hosted sessions is proven by three new test files; all pass. (1)
  apps/figcraft-server/tests/cloud_compat.rs starts figcraft-server in-process on loopback with a
  temp data dir and publishes a file with the server key. Desktop-app clients (engine Session +
  NetLink/Guest) then join the same session from its figcraft://join?host= link, its ws:// URL and a
  figcraft://join?server=ws:// link. All three get edit access and edits from each converge. The
  server then restarts on the same port: everyone reconnects, an edit made offline goes through, and
  clients resume with their old client ids, losing no confirmed edit. (2)
  crates/collab-net/tests/cloud_compat.rs checks a desktop-hosted session (collab_net::host::Host).
  The app's own client joins from both the host= link and the ws:// URL, and its edits reach the
  host's inbox. A browser-style client joins and gets a confirmed edit to the host; it sends an
  Origin and a browser User-Agent, uses the invite endpoint as its path, and speaks the protocol
  through figcraft_collab::Client. An older client whose client_version has no 'signals'/'hosting'
  words is still welcomed and is never sent Signal, Hosting or Reported. Hello.protocol=PROTOCOL+1
  is refused with a message naming both versions. A frame with another WIRE_VERSION is never decoded
  or welcomed. (3) crates/cloud-relay/tests/cloud_compat.rs (dev PostgreSQL+S3, fixture prefix f2)
  runs the same app client code against the cloud relay. It joins Hosting::serve(CloudBackend) from
  a cloud invite ws://<addr>/ws/v1/collab?file=<uuid>&token=<share secret>, and from that invite
  re-shared as figcraft://join?server=. Edits converge and the committed sequence in PostgreSQL is
  at least 2. After a relay restart on the same address both clients reconnect, catch up and push an
  offline edit, which raises the committed sequence. A viewer link joins with view access and is
  read-only. (4) All three link shapes round-trip through Invite::parse, link() and url(): host=,
  server=wss://…/path/, and cloud ws(s)://…/ws/v1/collab?file&token. A cloud link always re-shares
  as server=, never host=, keeps its /ws/v1/collab/ path, displays as a URL, is never read as a join
  code, and keeps its secret out of the endpoint. Errors about broken cloud links never echo the
  file id or secret. docs/saas/session-compatibility.md documents the compatibility matrix (client
  builds against desktop host, figcraft-server and cloud relay, with PROTOCOL=2 and WIRE_VERSION=1)
  and the rule for future protocol changes. That rule: additive changes behind client_version
  capability words; breaking changes bump PROTOCOL in the shared hub, servers first; WIRE_VERSION
  changes only with the encoding; link shapes are append-only; the cloud_compat tests are extended
  in the same commit.

### Failure and performance verification

- [x] **S5-020** Test two relay replicas owning different rooms.
  Evidence (2026-10-18): `two_replicas_own_different_rooms_at_the_same_time` runs two relay
  replicas on one database; each serves its own document through five confirmed commits per
  round, each leases its own room under its own endpoint, each refuses the other's room, and both
  documents end at committed sequence 5.
- [x] **S5-021** Test simultaneous lease acquisition and stale-owner fencing.
  Evidence (2026-10-18): `simultaneous_hellos_to_two_replicas_leave_exactly_one_owner` sends
  eight hellos at once (four to each replica, three rounds): exactly one replica holds the
  lease, all of its clients are served and every client of the other is refused. The storage-level
  three-case probe (simultaneous acquisition, expiry takeover, superseded fence) and the relay
  tests for a lease taken over and a fenced commit cover the stale owner.
- [ ] **S5-022** Kill relay before commit and prove no edit was confirmed.
- [ ] **S5-023** Kill relay after commit and prove the edit returns after reconnect.
- [x] **S5-024** Load-test reconnect storms, slow readers and maximum supported document size.
  Evidence (2026-10-17): `crates/cloud-relay/examples/relay_load.rs` drove a release relay against
  disposable PostgreSQL 18 and SeaweedFS S3 with 20 editors, 5 documents, 120 s and 2 batches/s.
  All 4,800 batches confirmed with no editor failures. The hub evicted two slow readers after 45 to
  50 s without delaying the others. Twenty simultaneous reconnects got their first commits within
  110 ms. The initial cloud limit is 20,001 nodes (one page plus 20,000 frames in the measured
  document); the 20k-frame cold open and edit succeeded at the default 15 s timeout. A second
  release run after applying that cap confirmed a 4,193 ms cold open at the boundary. The test
  `recovered_room_above_its_configured_node_limit_is_refused` verifies an oversized stored room is
  not served. `docs/saas/relay-performance.md` records the commands and measurements. Multiple
  replicas, production scale and clients in separate processes remain S5-GATE work.
- [x] **S5-025** Measure room recovery and commit latency against roadmap targets.
  Evidence (2026-10-17): the repeat release run recorded commit p95 3.7 ms for 4,800 edits,
  including the PostgreSQL commit and loopback client round trip, below the initial 50 ms server
  processing target at this scale. A fresh hub recovered a 20k-frame snapshot in 4,166 ms p95 and
  that snapshot plus 500 committed events in 4,235 ms p95 (three cold opens each, zero refusals),
  below the 10 s initial target at the configured cloud size limit. Removing a duplicate restore
  lowered the latter scenario from the 14,296 ms baseline. The bounded 20,001-node cloud limit is
  enforced on edits and recovered rooms; the editor core's 5M-node limit is separate. These are
  development-host measurements, not a production SLO; quiet-host and multi-replica evidence
  remains in S5-GATE. The complete baseline, repeat run, commands and sizing assumptions are in
  `docs/saas/relay-performance.md`.

### S5 gate

- [ ] **S5-GATE** Pass fenced multi-replica, crash, revocation, compatibility and load tests with no
  acknowledged-edit loss.

## S6 — Browser, desktop and embedded integration

### Host registration and tokens

- [x] **S6-001** Implement registered host systems, issuers, keys, audiences and allowed origins.
  Depends on S4-GATE and S5-GATE.
  Evidence: migration `20261018000001_host_systems.sql` (`host_systems`, `host_system_keys`, forced
  RLS, definer lookup `figcraft_host_issuer`), `crates/cloud-storage/src/host_systems.rs` and
  `crates/cloud-storage/tests/host_systems.rs` (register/read/update/disable, input validation of
  exact https origins and bounds, issuer and system-id conflicts, rotation overlap and retirement
  windows, live-key cap, cross-tenant RLS isolation; run live against PostgreSQL). Only public
  EdDSA/ES256 keys are stored (no HMAC hosts, no secrets at rest). Not done: the schema version
  gate constants in `apps/figcraft-cloud/src/main.rs`, `deploy/saas/smoke-preflight.sh` and docs
  still say `20261017000007` and must move to the newest migration (left to the S5 owner of
  `main.rs`); no admin API or UI to register hosts yet (S6-012).
- [x] **S6-002** Implement external tenant, workspace, principal and resource mappings.
  Evidence: `crates/cloud-storage/src/host_mappings.rs` and `tests/host_mappings.rs` (live PostgreSQL): a registered host maps its tenant, workspace, project, document and principal ids onto internal ids over the existing `external_mappings` table (no migration). Tests cover resolve, reverse lookup and unmap per kind; uniqueness per host and kind; refusal of empty, control-character and oversize external ids, unregistered hosts, duplicates (never re-pointed) and cross-tenant internal ids (one fixed NotFound); cross-tenant RLS isolation. Not done: no admin API to manage mappings (S6-012); principals are not auto-provisioned from `sub` claims; the per-host cap of 100000 mappings is checked in code and not load tested.
- [x] **S6-003** Implement short-lived embed-token validation and one-use `jti` consumption.
  Evidence: migration `20261018000002_embed_token_uses.sql`, `crates/cloud-auth/src/embed_token.rs` (pure `verify_embed_token`, `exchange_embed_token`), `crates/cloud-storage/src/embed_tokens.rs`, tests `embed_token` unit tests (6) and `crates/cloud-auth/tests/embed_exchange.rs` (4, live PostgreSQL). Covered: valid EdDSA and ES256 tokens; wrong issuer, audience, expired, not-yet-valid, over-long lifetime, bad or missing claims, bad signature, tampered claims, unknown or missing kid, alg none/HS256/RS256/case variants and key-algorithm mismatch, crit headers, oversize and malformed tokens; key not-before, expiry and retirement; replay refused; ten concurrent exchanges of one token give exactly one success; refused tokens do not burn their jti; disabled host refused; expired jti rows swept; RLS isolation. All refusals share the public message 'embed token refused'. Algorithms are EdDSA and ES256 only (no HMAC, no RSA); clock skew is a fixed 30 s and token lifetime at most 300 s. Not done: the role ceiling and mapped boundary (S6-004); the HTTP exchange endpoint and session minting (later S6 tasks); no per-host rate limit on exchanges yet.
- [x] **S6-004** Enforce host maximum role and mapped resource boundary.
  Evidence: `verify_embed_token` refuses a role above the host's registered maximum (unit test `role_never_exceeds_the_hosts_maximum`), and `exchange_embed_token` resolves the token's external tenant, workspace, document and subject through that host's mappings (`host_mappings::embed_boundary_holds`) before the jti is consumed. Live tests in `crates/cloud-auth/tests/embed_exchange.rs`: role escalation refused for viewer, commenter and editor ceilings without spending the jti, unknown role names refused; unmapped tenant, workspace, document and subject, raw internal UUIDs, a document in another workspace, an archived document, a suspended member and a tenant mapping pointing at another tenant all give the single Boundary refusal; two tenants with identical system and external ids resolve to their own rows only, and a cross-tenant issuer forgery fails the signature. Not done: the effective role is not yet intersected with the principal's own tenant/document grants (that belongs to the session minting step); principals are not auto-provisioned.
- [x] **S6-005** Add key rotation and issuer-disable behavior.
  Evidence: live tests in `crates/cloud-auth/tests/embed_exchange.rs`: rotation with overlap (new key refused before its not-before, both keys verify during the overlap, the old key refused from its expiry, the new key still works); retiring a key refuses its tokens from the retire instant; disabling a host refuses both unused and fresh tokens immediately (the issuer lookup hides disabled hosts, so no key is read) and re-enabling restores an unspent token exactly once; a suspended tenant behaves like a disabled issuer. Repository behaviour (overlap, retire, live-key cap of 8) is in `tests/host_systems.rs`. Not done: no admin API or UI to rotate keys (S6-012); no audit event is written for key changes or disables yet; verification keys are read per exchange with no cache, so there is no stale-key window but also no caching.

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
| 2026-10-17 | S3-018/S3-019/S3-020/S3-GATE: repeatable checkpointed legacy import with read-only cutover verification, a database-and-bucket-version backup/restore probe, and a Compose no-local-persistence check. S3 complete (20/20) and its gate passes. |
| 2026-10-17 | S2/S3 follow-ups: plugin uniqueness by index, attributed retention holds, document quarantine, register-first object writes with an orphan sweep, legacy library/plugin import, typed retryable event errors and stale idempotency-key takeover (four parallel agents; schema 20261017000003). Core slice section added: browser co-editing pulled ahead of full S4–S6. |
| 2026-10-17 | Core slice CS-001..CS-003: share links stored as hashed secrets; the cloud relay serves FigCraft's collaboration protocol on port 8082 (gateway `/ws/v1/collab`) with commit-before-broadcast durability, snapshot-first recovery and share-link admission; the browser editor is served at `/edit/` and joins from `#join=<link>`; `figcraft-cloud --create-share-link` prints a demo URL. CS-004 (two-browser demo through the stack) is running. |
| 2026-10-17 | Core slice CS-004: the product owner confirmed two browsers co-editing one cloud document through the Compose stack (real-time cursors, edits synced on commit). Follow-up: stream in-progress drags. Core slice complete. |
| 2026-10-17 | S4 round A and CS-005: axum API foundation with auth rate limits and the disclosure policy; OIDC login (openidconnect, PKCE, nonce, key rotation); browser sessions, bootstrap administrator and service accounts; deny-by-default effective-role authorization with memberships, team grants and explicit sensitive permissions; tenants, teams and invitations; share-link exchange with live revocation; tenant-key audit; runtime-configurable branding from Docker env. S4 reaches 14/25 (S4-017 and S4-021 partial). |
| 2026-10-17 | S4 round B: production authenticator and auth routes (OIDC login/callback, logout, bootstrap gate, share exchange), tenant/team/invitation/member and workspace/project/document APIs with HMAC cursors, row-level security on every tenant table with SECURITY DEFINER pre-tenant lookups, membership revocation closing sessions and relay connections, audited support access, job authorization recheck, cross-tenant and revocation race suites (which found and fixed a rotate-vs-revoke-all session race), identity-mapping policy draft and SAML procedure. S4 reaches 19/25. |
| 2026-10-17 | S4 round C (part) and S5 round A: revocation races at HTTP, WebSocket and job level (S4-023 partial: one in-flight HTTP order); SAML through a Keycloak broker end to end (S4-025 partial: back-channel logout, groups claim); orchestration decided as our own PostgreSQL leases; filesystem-server and desktop session compatibility proven (S5-019); S5-005/007/009/013 recorded as delivered by S2/S3 work. S5 reaches 5/25. |
| 2026-10-17 | S4 round C (rest) and S5 load harness: member-removal revocation hooks, signed cursors on document lists, `--print-bootstrap-token`, realtime tickets with relay admission; full route x caller permission matrix (1,152 cells) and API-level cross-tenant suite (S4-022); relay load and latency harness with measured sizing (S5-024/025 partial). Finding: commits and cold restores run serially on the relay hub thread (a 20k-node restore blocked other rooms for 8 s). S4 at 21/25, S5 at 5/25. |
| 2026-10-17 | S4 complete (25/25) and its gate passes: team-member search and workspace grant listing with full audit; relay action matrix; resume bound to the issuing credential (a viewer could otherwise take over an editor's client id); every mutating request re-validates access inside its transaction; tenant locks moved to FOR NO KEY UPDATE after a deadlock with audit inserts; OIDC back-channel logout and a recorded groups claim; Keycloak image pinned for validation. |
| 2026-10-17 | S5-002: bound each accepted relay batch to the principal of its own admitted client, rather than the room opener; a two-editor PostgreSQL/S3 regression verifies per-event actors and commit order. S5 reaches 6/25. |
| 2026-10-17 | S5-025 follow-up: removed a duplicate server restore from cold room recovery; comparable latency measurement remains open. S5 stays 6/25. |
| 2026-10-17 | S5-024/025: repeated the release load on disposable PostgreSQL/S3 after the recovery fix: 4,800/4,800 edits confirmed, commit p95 3.7 ms, 20k-frame plus 500-event recovery p95 4.24 s, two slow readers evicted, 20 reconnects committed within 110 ms. Capped cloud rooms at the measured 20,001 nodes and verified both the boundary and an oversized stored snapshot. S5 reaches 8/25; production-scale and multi-replica tests remain in the gate. |
| 2026-10-17 | S5 lease prerequisite: corrected cloud startup and Compose preflight to require the checked-in `20261017000006` row-level security migration; the previous `...05` bound left a fully migrated service unready. S5 remains 8/25. |
| 2026-10-17 | S5-003/004 partial: added tenant-scoped PostgreSQL lease rows, atomic acquisition with a fencing claim, lookup, renewal and compare-and-release. Live relay integration and routing remain open; S5 stays 8/25. |
| 2026-10-17 | S5-015 audit: confirmed the already wired relay revocation sweeper with live PostgreSQL/S3 member and WebSocket race probes; checked off grant revocation. S5 reaches 9/25. |
| 2026-10-17 | S5-016 audit: confirmed separate presence and signal throttles in the shared live hub and that only accepted edits reach the cloud journal. S5 reaches 10/25. |
| 2026-10-17 | S5-014 audit: the live PostgreSQL/S3 relay action matrix passed for editor, viewer and commenter tickets and links, including forged edit access and client-ID resume. S5 reaches 11/25. |
| 2026-10-17 | S5 prerequisite: room restore moved off the relay hub thread (additive `Backend::begin_open`/`opened`/`attach` hooks, patch FP-008). Measured: another room's worst commit during a 20k-frame cold open fell from 4,246 ms to 6.1 ms. No S5 task changes state; S5 stays 11/25. |
| 2026-10-18 | S5-003, 004, 010, 020, 021: the relay takes, renews, loses and releases PostgreSQL room leases (opt-in with `FIGCRAFT_RELAY_ADVERTISE_URL`); two live replicas on one database each own their rooms and refuse the other's, a simultaneous hello race leaves one owner, and a taken-over room closes without confirming more. Owner routing (S5-006) and drain (S5-012) remain. S5 reaches 16/25. |
| 2026-10-18 | S5-006: a replica passes a client on to the room's owner (relay-to-relay proxy over the lease's endpoint) instead of refusing it; no client, protocol or gateway change. Revocation, two-replica and race tests pass live. S5 reaches 17/25. |
| 2026-10-18 | S5-012: SIGTERM/SIGINT drain for rolling releases (not ready, new relay connections turned away with the restarting code, grace period, then commit, snapshot, lease release and a restarting close for connected clients); verified against the real process. S5 reaches 18/25. |
| 2026-10-18 | S5-011: emptied rooms stay warm for a configurable period before closing (saved, lease released); busy rooms snapshot by batch count or time instead of only on close. Hub housekeeping tick added (patch FP-008). S5 reaches 19/25. |
| 2026-10-18 | S6-001: registered embedding host systems (issuer, audiences, exact https parent origins, max role, enabled flag) with rotating public EdDSA/ES256 keys, tenant RLS and a pre-tenant issuer lookup. S6 reaches 1/31. |
| 2026-10-18 | S5-001: audited realtime tickets and covered them through the owner-routing path (a ticket at a non-owning replica is checked, passed on and checked again by the owner; attribution and access hold). S5 reaches 20/25. |
| 2026-10-18 | S6-002: host-managed external id mappings (tenant, workspace, project, document, principal) that require a registered host and a same-tenant internal resource, with bounds and cross-tenant refusal. S6 reaches 2/31. |
| 2026-10-18 | S6-003: embed-token validation (EdDSA/ES256 against registered rotating keys, issuer, audience, expiry, not-before, skew, lifetime and size bounds) and atomic one-use jti consumption in PostgreSQL (schema 20261018000002). S6 reaches 3/31. |
| 2026-10-18 | S6-004: embed exchange enforces the host's maximum role and resolves tenant, workspace, document and subject only through that host's mappings inside its own tenant (active document in the mapped workspace, active member). S6 reaches 4/31. |
| 2026-10-18 | S6-005: key rotation (overlap, expiry, retirement) and issuer disable verified end to end through the embed exchange against PostgreSQL. S6 reaches 5/31 and the first S6 block (host registration and tokens) is complete except the unstarted embed behaviour tasks. |

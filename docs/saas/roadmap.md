# SaaS roadmap

> **Status:** planned · **Last updated:** 2026-10-16 · **Estimates:** one experienced engineer,
> implementation and tests included

## Outcome stages

| Stage | Outcome | Milestones |
|---|---|---|
| Architecture baseline | Reviewed scope, decisions, tracker and fork policy | S0 |
| Private preview | One tenant runs through Docker with PostgreSQL and MinIO | S1–S3 |
| Multi-tenant alpha | Accounts, workspaces, roles and isolated storage | S4 |
| Collaborative beta | Horizontally scalable, fenced realtime rooms | S5 |
| Product integration beta | Browser and desktop use cloud files; host products embed them | S6 |
| AI automation beta | Governed AI and server-side WASM jobs | S7 |
| Production candidate | Quotas, billing, operations and security gates | S8 |
| Production | Restore-tested, monitored rollout with support procedures | S9 |

## Dependency graph

```text
S0 Documentation and fork governance
 |
S1 Cloud foundation
 |
S2 PostgreSQL metadata and event storage
 | \
 |  S3 MinIO assets and immutable snapshots
 |   \
S4 Identity, tenancy and authorization
 |     \
S5 Distributed collaboration relay
 |       \
S6 Embedded integration    S7 AI and server WASM
 \                         /
  S8 Operations, quotas, billing and security
   |
  S9 Production rollout
```

Security, observability and upstream synchronization run through every milestone even though their
final release gate is S8.

## Milestone summary

| ID | Milestone | Estimate | Depends on | Exit outcome |
|---|---|---:|---|---|
| S0 | Documentation and fork governance | 8–16 h | — | Architecture, requirements, tracker and upstream policy approved |
| S1 | Cloud foundation | 40–65 h | S0 | Cloud roles and the fork-owned browser shell build with health and configuration |
| S2 | PostgreSQL metadata and events | 60–100 h | S1 | Files and ordered collaboration events survive process and database-safe restarts |
| S3 | MinIO objects and migration | 45–75 h | S1, S2 | Snapshots, assets, libraries, plugins and exports use verified immutable objects |
| S4 | Identity, tenancy and authorization | 100–165 h | S2 | Single and multiple tenants use the same isolated schema and role model |
| S5 | Distributed collaboration relay | 100–170 h | S2, S3, S4 | Multiple relay replicas safely own rooms with leases and fencing |
| S6 | Browser, desktop and embedded integration | 95–160 h | S3, S4, S5 | All clients share cloud identity and host products embed authorized documents |
| S7 | AI gateway and server WASM | 100–170 h | S3, S4, S5 | Governed AI and automation apply safe, auditable command batches |
| S8 | Operations, quotas, billing and security | 110–185 h | S4–S7 | Production controls, restore, observability and security gates pass |
| S9 | Production rollout | 45–80 h | S8 | Staged customer rollout, support, SLOs and rollback are operational |

Total planning range: **703–1,186 engineer-hours**. A private single-tenant preview through S3 is
approximately **153–256 hours**. Estimates exclude native mobile applications, active-active
multi-region editing and building a foundation model.

## S0 — Documentation and fork governance

### Deliverables

- Product requirements and non-goals.
- Target architecture and consistency model.
- Data, tenancy, embedding, AI/WASM, deployment and security specifications.
- Upstream synchronization policy and patch ledger.
- Milestone roadmap and task tracker.
- Named reviewers for architecture, security and operations before implementation begins.

### Exit gate

- Documentation links resolve.
- No section relies on an unstated placeholder implementation.
- Major architecture decisions have explicit reasons.
- Initial task dependencies and release gates are agreed.
- Fork remote and branch policy are understood by maintainers.

## S1 — Cloud foundation

### Deliverables

- Fork-owned cloud crate and application layout.
- Shared tenant-aware domain identifiers and error model.
- `figcraft-cloud` role selection.
- Strict configuration with secret-file support.
- Structured logging, request identifiers and tracing foundation.
- Liveness, readiness and startup endpoints.
- Multi-stage non-root container image.
- Initial Docker Compose stack with PostgreSQL, Redis and MinIO dependency health.
- Cloud CI jobs that do not weaken existing upstream gates.
- Fork-owned React/TypeScript/Vite workspace with pinned toolchain, lockfile and quality gates.
- Versioned TypeScript-to-Rust/WASM bridge contract.

### Exit gate

- One image starts in every role.
- Compose reaches healthy state from an empty machine with documented prerequisites.
- Unknown configuration fails startup.
- Production mode rejects development credentials.
- Existing desktop, web, CLI and filesystem server builds remain green.
- The browser shell passes formatting, typecheck, lint, tests, production build and dependency
  license checks.

## S2 — PostgreSQL metadata and event storage

### Deliverables

- Migration framework and schema-version compatibility checks.
- Tenant, workspace, project and document metadata repositories.
- Ordered event journal and document sequence transaction.
- Snapshot metadata and object-pointer records.
- Durable jobs, idempotency keys and audit-event foundations.
- Database-backed implementation of collaboration durability behavior.
- Import reader for existing filesystem server metadata and journals.
- PostgreSQL integration tests for concurrency, failures and recovery.

### Exit gate

- A document can be created, opened, edited and restored using PostgreSQL metadata and events.
- The server never broadcasts an edit whose event transaction failed.
- Concurrent sequence appends cannot duplicate or skip an acknowledged sequence.
- Restart replay reproduces the accepted document state.
- Migrations work from empty and the previous supported schema.

## S3 — MinIO objects and migration

### Deliverables

- Object repository and content-addressed key policy.
- Upload reservation and verification flow.
- Immutable document snapshots with checksum validation.
- Asset, team-library, plugin, export and thumbnail object types.
- Presigned download authorization.
- Object-reference tracking and garbage collection.
- MinIO initialization, versioning and lifecycle policy.
- Filesystem-to-PostgreSQL/MinIO migration command with dry-run and report.
- Backup and restore integration test.

### Exit gate

- No new cloud snapshot, asset, library, plugin or export depends on local container storage.
- A corrupted or mismatched object is rejected and does not replace the last verified snapshot.
- Abandoned uploads expire.
- Migration can be repeated without duplicating resources.
- A restored database and bucket reopen sampled documents correctly.

## S4 — Identity, tenancy and authorization

### Deliverables

- OIDC browser sessions.
- Enterprise federation through an OIDC identity broker, with documented claim mapping.
- Service accounts and scoped credentials.
- Single-tenant bootstrap flow.
- Tenant, workspace and document membership.
- Roles and explicit sensitive permissions.
- PostgreSQL row-level security.
- Revocable viewer, commenter and editor share links.
- Tenant quotas and plan-policy primitives.
- Tenant-aware API pagination and search.
- Cross-tenant adversarial test suite.

### Exit gate

- Single and multi-tenant configurations run the same schema and APIs.
- Every route and repository operation has authorization coverage.
- Cross-tenant identifiers cannot read, change or disclose resource existence beyond policy.
- Membership or share revocation terminates active access.
- Tenant suspension stops new writes without corrupting stored documents.

## S5 — Distributed collaboration relay

### Deliverables

- Realtime ticket endpoint.
- Redis room registry and renewable lease.
- PostgreSQL fencing tokens.
- Relay owner routing and reconnect flow.
- Snapshot restore plus event replay.
- Durable commit-before-broadcast implementation.
- Room drain and graceful rolling restart.
- Asset-reference collaboration changes for large binary data.
- Rate, connection, queue and room quotas.
- Split-brain, relay-crash and reconnect-storm tests.

### Exit gate

- At least two relay replicas serve different rooms.
- No test can make two fencing tokens append valid events concurrently.
- Killing a relay before or after commit loses no acknowledged edit.
- View and comment roles are enforced at the relay.
- Rolling restart reconnects clients without manual intervention.
- Room recovery meets the initial recovery target for supported document sizes.

## S6 — Browser, desktop and embedded integration

### Deliverables

- React/TypeScript workspace navigation for recents, projects, libraries, search, notifications
  and settings, isolated under `webapp/`.
- Stable workspace/document routes and one cloud document identity across browser and desktop.
- Typed TypeScript-to-Rust/WASM editor lifecycle and collaboration bridge.
- Browser cloud open, edit, reconnect and export flows.
- Desktop local/cloud repository adapter and workspace browser.
- Visible saved, saving, offline, conflict and error states.
- Durable desktop offline journal and recoverable conflict branches.
- Registered host systems and issuer policy.
- One-use embed-token exchange.
- External tenant, workspace, user and resource mappings.
- Origin-restricted iframe handshake.
- Versioned document-management API and OpenAPI output.
- Idempotent create and export operations.
- Signed webhook endpoints, retries, replay and delivery inspection.
- Host integration example and contract tests.

### Exit gate

- Browser and desktop open the same cloud identifier and converge on the same accepted sequence.
- Offline desktop edits reconcile or remain available in a recoverable conflict branch.
- A sample host creates and embeds a document without sharing internal credentials.
- Removing host access closes the embedded session.
- Origin and token-replay attacks are rejected.
- Webhook signatures verify, retries remain idempotent and terminal failures are visible.
- API compatibility policy is documented and tested.

## S7 — AI gateway and server WASM

### Deliverables

- Provider-neutral text, structured, vision, image and embedding adapters.
- Tenant provider policy and encrypted BYOK configuration.
- Usage reservation, settlement and cost records.
- Context minimization and retention modes.
- Structured command proposal and validation.
- AI service participant and relay submission.
- Generated-asset verification and storage.
- Dedicated server-WASM worker with capability model and limits.
- AI and WASM audit views, cancellation and operator controls.
- Adversarial model-output and module tests.

### Exit gate

- AI and WASM cannot bypass authorization or durable collaboration.
- Failed work leaves the document unchanged.
- Provider secrets do not appear outside the AI role.
- Tenant budgets hold across retry and cancellation.
- Generated assets are unavailable until verification.
- Disabling a provider or module prevents new executions immediately.

## S8 — Operations, quotas, billing and security

### Deliverables

- Usage aggregation and enforceable quotas.
- Subscription integration behind a billing-provider abstraction.
- Tenant administration and operator support controls.
- SCIM 2.0 provisioning and enterprise identity-lifecycle audit coverage.
- OpenTelemetry traces, production metrics, dashboards and alerts.
- Database and object-store backup automation.
- Restore drills and disaster-recovery evidence.
- Security scanning, software bill of materials and signed images.
- Operational runbooks and incident response.
- Load, soak, tenant-isolation and external penetration testing.
- Kubernetes role-separated deployment after Compose behavior is stable.
- Docker Swarm packaging when required by a supported managed deployment.

### Exit gate

- Production security gate in [security.md](security.md) passes.
- Backup restoration meets declared recovery objectives.
- Quotas prevent one tenant from exhausting shared resources in load tests.
- Alerts identify relay, database, object, job, webhook and AI failures.
- Rolling upgrade and supported rollback are demonstrated.
- Billing failures cannot delete or corrupt customer documents.

## S9 — Production rollout

### Deliverables

- Internal staging with production-equivalent dependencies.
- Design-partner tenant rollout.
- Support, status and incident communication procedures.
- Published service limits, retention, privacy and subprocessor behavior.
- SLO monitoring and error-budget policy.
- Capacity model and scaling thresholds.
- Release, rollback and emergency-upstream processes exercised.
- General-availability readiness review.

### Exit gate

- Design partners complete create, edit, collaborate, reconnect, export and restore journeys.
- No unresolved critical tenant-isolation or durability defect remains.
- On-call operators can execute core runbooks.
- SLOs are measured from real service telemetry.
- The included upstream commit and fork changes are traceable for the release.

## Release slices

### Private preview

S1–S3 provide Docker deployment, database metadata/events and MinIO objects for one configured
tenant. Authentication may initially use one OIDC issuer, but public unauthenticated administration
is not permitted.

### Multi-tenant alpha

S4 adds users, organizations, workspace roles, share links and row-level isolation. It is suitable
for controlled internal tenants, not public signup.

### Collaborative beta

S5 adds safe horizontal realtime scaling and production-shaped restart behavior.

### Embedded beta

S6 exposes the host-product contract, one-use tokens and webhooks.

### AI automation beta

S7 enables selected AI features and approved server automation under quotas and audit.

### Production candidate and production

S8 proves operational and security controls. S9 validates the service with staged users and makes
support and reliability commitments measurable.

## Progress updates

When work lands:

1. Check the task in [tracker.md](tracker.md).
2. Add evidence: tests, migration version, endpoint, screenshot or runbook.
3. Update the milestone status and estimate when scope changes materially.
4. Add a patch-ledger entry when an upstream-owned file changes.
5. Record a new architecture decision when implementation changes a decision in
   [README.md](README.md).
6. Keep one delivery arc per commit with its SaaS milestone and task ID in the commit subject.

# SaaS product requirements

> **Status:** baseline · **Last updated:** 2026-10-16

## Product objective

Provide FigCraft as a commercially operable design workspace that can be deployed by one customer,
hosted as a multi-tenant service, or embedded in another product. All modes use the same application
protocols, tenant-aware schema and container image.

## Users and operators

| Persona | Needs |
|---|---|
| Designer | Create, edit, share and present documents with low latency and offline recovery |
| Reviewer | View, comment and inspect without receiving edit permission |
| Team manager | Manage a team roster and the projects, documents and libraries granted to it |
| Workspace administrator | Manage members, roles, projects, retention, provider policy and usage |
| SaaS operator | Provision tenants, enforce quotas, diagnose incidents, bill usage and restore data |
| Host-product developer | Create documents, embed the editor, map permissions and receive lifecycle events |
| Automation developer | Use scoped APIs, MCP-compatible workflows, jobs or WASM without bypassing authorization |
| AI feature | Read authorized context, create auditable proposals or edits, and consume tenant quotas |

## Deployment modes

### Single tenant

- One configured tenant owns all workspaces.
- Local accounts or OIDC may authenticate users.
- Public deployments must not permit unauthenticated administrative access.
- The same PostgreSQL schema includes the tenant identifier on every resource.
- The all-in-one cloud role is supported for a small installation.
- An operator can later enable multi-tenant mode without converting document data.

### Multi-tenant SaaS

- Users may belong to several tenants, teams and workspaces.
- Tenant selection is explicit and validated on every request.
- PostgreSQL row-level security backs application authorization.
- Object keys and cache keys include the tenant identifier.
- Plans control storage, collaborators, AI usage, export work and retention.
- Operator support access is time-limited, audited and disabled by default.

### Embedded workspace

- A host product maps an external tenant and workspace to FigCraft resources.
- Its backend creates short-lived, audience-bound embed tokens.
- Tokens identify a principal, tenant, workspace, document, role and expiry.
- The browser exchanges a one-use token for a FigCraft session.
- The host can create, open, archive and export documents through a versioned API.
- Webhooks notify the host of document, comment, version, export and AI-job events.
- The host cannot supply arbitrary internal tenant identifiers without a registered mapping.

## Functional requirements

### Identity and access

- Support OIDC authorization-code flow with PKCE for interactive users.
- Support enterprise SAML through an identity broker that produces the same validated identity
  context; implement native SAML only when a reviewed customer requirement cannot use the broker.
- Support SCIM 2.0 user and group provisioning in the enterprise phase.
- Support service accounts for backend integrations.
- Support short-lived embed tokens from registered issuers.
- Support roles: tenant owner, tenant admin, team manager, workspace admin, editor, commenter and
  viewer.
- Support teams as first-class tenant-scoped groups with their own membership, invitations and
  resource grants.
- Permit teams to own or receive access to workspaces, projects, documents and shared libraries;
  direct user grants remain available for exceptional access.
- Support document-specific grants and revocable share links.
- Record security-relevant actions in an immutable audit trail.
- Revoke sessions after membership removal or tenant suspension.

### Workspaces and documents

- Create, rename, archive and restore teams within one tenant.
- Add, remove and invite team members without duplicating their user identities.
- Make effective access inherit from team grants and revoke that inherited access when a person
  leaves or is removed from the team.
- Create, list, rename, archive, restore and delete projects and documents.
- Move, duplicate, favorite, search and trash documents without changing their cloud identity.
- Persist current document state, ordered events and immutable snapshots.
- Preserve existing `.figcraft` import and export.
- Generate thumbnails asynchronously.
- Support configurable retention and deletion grace periods.
- Permit document ownership transfer within a tenant.
- Prevent resource movement across tenants except through an explicit export/import operation.

### Browser product experience

- Use React and TypeScript for the fork-owned SaaS shell, routing, identity screens, workspace
  management and browser integration after the root repository policy defines this exception.
- Keep the editor engine, document state, rendering, commands, reconciliation and undo in the
  existing Rust/WASM application.
- Provide workspace navigation for recents, projects, shared libraries, search, notifications and
  settings.
- Use `/workspace/{workspace_id}/file/{document_id}` as the stable editor route, subject to the
  public API's final identifier encoding.
- Preserve one cloud document identifier across browser, desktop, API, realtime and webhook
  surfaces.
- Expose a typed TypeScript-to-Rust/WASM host contract for mount, open, close, local changes,
  remote changes, connection state, selection and export.
- Meet WCAG 2.2 AA for the workspace shell and include the editor in keyboard and screen-reader
  audits.

### Desktop cloud documents

- Keep local `.figcraft` files and cloud documents as explicit repository modes.
- Browse and open authorized cloud workspaces without importing a new unrelated local copy.
- Display `saved`, `saving`, `offline`, `conflict` and `error` state for every cloud document.
- Persist offline changes locally before presenting them as safely queued.
- Reconcile stable operation identifiers after reconnect.
- Preserve unmergeable local work as a recoverable conflict branch with a visible user choice.

### Libraries, review and history

- Publish immutable versions of workspace components, styles, variables and asset references.
- Make library subscription updates explicit and preserve instance overrides.
- Support anchored comments, replies, mentions, resolution and notification preferences.
- Present named versions and collaboration checkpoints without confusing them with event-log
  compaction snapshots.
- Restore an earlier version by creating a new accepted head while retaining the original history.

### Collaboration

- Keep one authoritative sequence for each document.
- Preserve optimistic local edits, per-user undo and reconnect behavior.
- Persist accepted changes before confirmation.
- Route reconnecting clients to the current authoritative worker.
- Recover a room from the latest verified snapshot and later events.
- Support presence, cursors, selections, cursor chat and spotlight as ephemeral traffic.
- Enforce view, comment and edit permissions at the relay, not only in the UI.
- Disconnect users whose grant or share link is revoked.

### Assets and object storage

- Upload large assets directly with short-lived presigned requests.
- Address reusable objects by a cryptographic content hash.
- Verify declared size, checksum, media type and tenant before attaching an object.
- Track object references so unused objects can be collected safely.
- Keep snapshots, libraries, plugin bundles, exports and thumbnails immutable.
- Support MinIO in Docker and any compatible S3 service in production.

### AI

- Route text, vision and image requests through a provider-neutral gateway.
- Support platform-managed providers, tenant bring-your-own-key providers and local endpoints.
- Enforce per-tenant feature policy, rate limits and spend limits.
- Build model context from authorized document data only.
- Apply edits as named command batches through the authoritative collaboration path.
- Store generated binary assets in object storage before adding their references.
- Record provider, model, feature, usage, outcome and actor without logging provider secrets.
- Let tenants configure prompt and response retention.

### WASM automation

- Keep existing client plugins compatible.
- Run server automation only in a dedicated worker role.
- Enforce fuel, memory, input, output and wall-clock limits.
- Deny filesystem and raw network access by default.
- Provide explicit document, asset, logging and AI capabilities.
- Attribute mutations to a service identity and one undoable command batch.

### Integration API

- Version all public routes under `/v1`.
- Publish an OpenAPI document generated or verified from Rust route definitions.
- Use stable opaque identifiers.
- Require idempotency keys for externally retried creates and jobs.
- Return a request identifier with every response and error.
- Sign webhooks and retry them with bounded exponential backoff.
- Expose job status for exports, imports, thumbnails and AI operations.

### Operations

- Provide Docker Compose deployment for evaluation and single-server production.
- Provide role-separated containers from the same image.
- Run database migrations as an explicit release step.
- Expose liveness, readiness and dependency health endpoints.
- Export structured logs, metrics and OpenTelemetry traces.
- Back up PostgreSQL and object storage and test restoration.
- Support zero-data-loss relay restart for confirmed edits.

## Non-functional requirements

| Area | Initial target |
|---|---|
| Availability | 99.9% monthly for the hosted control plane after production launch |
| Confirmed-edit durability | No acknowledged edit lost after a single process or node failure |
| Tenant isolation | No cross-tenant read, write, cache or object access in automated adversarial tests |
| Relay latency | p95 server processing under 50 ms, excluding client network latency and snapshot loading |
| API latency | p95 under 300 ms for metadata operations not waiting on background work |
| Room recovery | p95 under 10 seconds for documents within the configured size limit |
| Horizontal scale | Add relay capacity without moving stored document data manually |
| Backup | Configurable RPO up to 15 minutes and RTO up to 4 hours for the initial hosted service |
| Auditability | Actor and tenant recorded for permission, sharing, provider and destructive operations |
| Portability | Development and single-node deployment run through Docker Compose |
| Accessibility | Workspace shell targets WCAG 2.2 AA; editor keyboard and screen-reader behavior is audited |
| Cross-client identity | Browser and desktop operate on the same cloud document identifier and accepted sequence |

These targets are release gates, not claims about the current repository.

## Non-goals for the first SaaS release

- Moving canvas rendering or normal command execution to a remote rendering service.
- Active-active editing of the same document in several regions.
- Running Figma JavaScript plugins.
- Giving plugins arbitrary internet access.
- Storing confirmed collaboration state only in Redis.
- Using MinIO as the searchable metadata database.
- Supporting anonymous administrative access.
- Building native mobile applications.
- Providing a general-purpose user-code hosting platform.

## Product acceptance journeys

### Private deployment

An operator starts the Compose stack, signs in through the configured identity provider, creates a
workspace, opens the browser editor, shares an edit link, restarts the application containers and
continues from the last confirmed edit.

### Hosted tenant

A tenant owner creates a workspace, invites an editor and a viewer, sets a storage and AI policy,
and observes isolated usage. Revoking the viewer immediately closes their live connection and
invalidates subsequent requests.

### Embedded document

A host backend creates a document with an external resource mapping, issues a one-use editor token,
and embeds the editor. The user edits collaboratively. The host receives a signed document-updated
webhook and requests a PNG export through an idempotent job.

### AI edit

An authorized editor asks the AI to organize selected layers. The gateway sends only the permitted
selection context, the model returns a proposed command batch, validation succeeds, and the relay
commits the edit as a visible AI participant with one undo step and an audit record.

### Recovery

A relay worker terminates after accepting an edit. Because the database commit preceded the
confirmation, another worker acquires a higher fencing token, restores the snapshot, replays events
and reconnects clients without losing the confirmed edit.

# Data, tenancy and integration

> **Status:** target design · **Last updated:** 2026-10-16

## Identifier policy

- Use randomly generated UUIDs for cloud resources.
- Treat identifiers as opaque outside their owning service.
- Include `tenant_id` on every tenant-owned row even when a parent already implies it.
- Include `tenant_id` in uniqueness constraints where names or external mappings are scoped.
- Never derive MinIO keys directly from unsanitized customer names.
- Retain existing FigCraft node and collaboration identifiers inside document state.

## PostgreSQL schema

### Identity and tenancy

| Table | Required fields |
|---|---|
| `principals` | `id`, `kind`, `display_name`, `state`, timestamps |
| `identity_links` | `principal_id`, `issuer`, `subject`, verified metadata |
| `tenants` | `id`, `slug`, `name`, `state`, `plan_id`, `home_region`, timestamps |
| `tenant_members` | `tenant_id`, `principal_id`, `role`, `state`, timestamps |
| `workspaces` | `id`, `tenant_id`, `name`, `state`, timestamps |
| `workspace_members` | `tenant_id`, `workspace_id`, `principal_id`, `role`, timestamps |
| `service_accounts` | `id`, `tenant_id`, `name`, `state`, credential metadata |
| `external_mappings` | `tenant_id`, `system_id`, `resource_kind`, `external_id`, `internal_id` |

`identity_links` permits one principal to use several OIDC issuers without duplicating tenant
membership. External mappings are unique by tenant, registered system, resource kind and external
identifier.

### Projects and documents

| Table | Required fields |
|---|---|
| `projects` | `id`, `tenant_id`, `workspace_id`, `name`, `state`, timestamps |
| `documents` | `id`, `tenant_id`, `workspace_id`, `project_id`, `name`, `state`, `current_seq`, `fencing_token`, snapshot pointer, timestamps |
| `document_members` | `tenant_id`, `document_id`, `principal_id`, `role`, timestamps |
| `share_links` | `id`, `tenant_id`, `document_id`, `token_hash`, `role`, expiry, revocation, timestamps |
| `document_events` | `tenant_id`, `document_id`, `seq`, `fencing_token`, payload, checksum, actor, timestamp |
| `document_snapshots` | `id`, `tenant_id`, `document_id`, `seq`, `object_id`, checksum, size, state, timestamp |
| `document_versions` | `id`, `tenant_id`, `document_id`, `seq`, `name`, actor, timestamp |
| `document_comments` | metadata needed for indexed comment views; canonical synchronized content remains in document state until deliberately extracted |

Required constraints include unique `(tenant_id, document_id, seq)` events, monotonic document
sequence updates and one active snapshot pointer belonging to the same document and tenant.

### Objects and assets

| Table | Required fields |
|---|---|
| `objects` | `id`, `tenant_id`, `kind`, `bucket`, `key`, `sha256`, `size`, media type, state, timestamps |
| `object_references` | `tenant_id`, `object_id`, `owner_kind`, `owner_id`, timestamps |
| `upload_reservations` | `id`, `tenant_id`, expected hash/size/type, staging key, expiry, state |
| `exports` | `id`, `tenant_id`, `document_id`, requested sequence/options, job, result object, state |
| `libraries` | `id`, `tenant_id`, source document, name, state |
| `library_versions` | `id`, `tenant_id`, `library_id`, version, object, checksum, timestamp |
| `plugins` | `id`, `tenant_id` when private, publisher, name, state |
| `plugin_versions` | `id`, plugin, version, object, checksum, manifest, state |

Objects are immutable after verification. Replacement creates a new object and changes references.
Garbage collection removes an object only after no live reference, retention hold or pending job
needs it.

### Jobs, AI and operations

| Table | Required fields |
|---|---|
| `jobs` | `id`, `tenant_id`, kind, input, state, priority, lease, attempts, result, error, timestamps |
| `ai_provider_configs` | `id`, `tenant_id`, provider kind, encrypted secret reference, policy, state |
| `ai_usage` | tenant, principal, feature, provider, model, units, cost, outcome, request/job, timestamp |
| `plans` | limits and feature policy |
| `tenant_usage` | tenant, period, storage, collaborators, exports, AI units |
| `subscriptions` | tenant, provider identifiers, status, period |
| `webhook_endpoints` | tenant, URL, encrypted signing secret, event filter, state |
| `webhook_deliveries` | endpoint, event, attempt, response metadata, next attempt, state |
| `audit_events` | tenant, actor, action, resource, request, IP metadata, outcome, timestamp |
| `idempotency_keys` | tenant, principal, route, key hash, request hash, response reference, expiry |

Provider secrets are referenced through encrypted secret storage. They do not appear in normal
configuration responses, job inputs, logs or audit payloads.

## Row-level security

Each request transaction sets a database-local tenant variable after authentication. Row-level
security policies require tenant equality for reads and writes. Operator maintenance uses a
separate database role and explicit audited workflows.

Repository methods still include tenant predicates. Row-level security is defense in depth, not a
replacement for application authorization.

Tests must attempt cross-tenant access through:

- Direct resource identifiers.
- Parent-child mismatches.
- Search and pagination cursors.
- Job identifiers and result objects.
- Share-link and embed-token reuse.
- MinIO presigned requests.
- Redis room and cache keys.
- Webhook delivery inspection.
- Operator support tooling.

## MinIO object layout

```text
tenants/${TENANT_ID}/documents/${DOCUMENT_ID}/snapshots/${SEQUENCE}-${SHA256}.bin
tenants/${TENANT_ID}/assets/${SHA256}
tenants/${TENANT_ID}/libraries/${LIBRARY_ID}/${VERSION}-${SHA256}.figcraft
tenants/${TENANT_ID}/plugins/${PLUGIN_ID}/${VERSION}-${SHA256}.bin
tenants/${TENANT_ID}/exports/${EXPORT_ID}/${FILENAME}
tenants/${TENANT_ID}/thumbnails/${DOCUMENT_ID}/${SEQUENCE}.webp
tenants/${TENANT_ID}/staging/${RESERVATION_ID}
```

Object metadata records tenant, object kind, checksum, content type and producing software version.
Presigned requests expire quickly and permit one method on one key. Downloads are authorized before
signing. Staging objects have lifecycle expiry even when a completion request never arrives.

## Authentication modes

### OIDC user session

- Authorization-code flow with PKCE.
- Validate issuer, audience, signature, nonce, state and time claims.
- Map `(issuer, subject)` to an internal principal.
- Use secure, HTTP-only, same-site cookies for the browser session.
- Rotate session identifiers after authentication and privilege changes.

### Enterprise federation and provisioning

- Federate SAML through a reviewed identity broker that exposes the same OIDC session contract.
- Map issuer, subject, verified domain and configured group claims to internal identities and roles;
  never accept a tenant identifier or administrative role solely because the client supplied it.
- Keep native SAML out of the application unless an approved customer requirement cannot be met by
  the broker.
- Implement SCIM 2.0 users and groups after membership semantics stabilize.
- Make SCIM create, update, suspend and delete operations idempotent and audit every result.
- Deprovisioning revokes sessions, realtime tickets, service delegation and pending privileged
  work according to policy.

### Service account

- Store only a strong token hash or validate a signed client assertion.
- Scope credentials to a tenant, workspace and action list.
- Support explicit expiry and rotation overlap.
- Do not permit interactive tenant-owner actions unless separately granted.

### Embed token

Required claims:

| Claim | Meaning |
|---|---|
| `iss` | Registered host-product issuer |
| `aud` | FigCraft deployment audience |
| `sub` | External user identifier |
| `jti` | Unique token identifier for one-use exchange |
| `exp`, `iat`, `nbf` | Short validity interval |
| `tenant` | Registered external tenant mapping |
| `workspace` | Registered external workspace mapping |
| `document` | Internal or registered external document mapping |
| `role` | Maximum role for this session |

The exchange endpoint consumes `jti`, resolves registered mappings and issues a FigCraft session.
The raw embed token is not retained in browser storage or sent on subsequent requests.

### Share link

- Generate at least 128 random bits.
- Store a cryptographic hash, not the presented token.
- Permit viewer, commenter or editor access.
- Support expiry, labels and immediate revocation.
- Exchange for a short-lived session rather than sending the permanent token on every WebSocket
  frame.

## Authorization model

Effective access is the highest permitted role from tenant membership, workspace membership,
document membership or the active share grant, constrained by tenant and document state.

| Role | Core abilities |
|---|---|
| Tenant owner | Billing, tenant policy, administrators, export and deletion |
| Tenant admin | Members, workspaces, projects, provider policy and audit views |
| Workspace admin | Workspace members, projects and workspace documents |
| Editor | Read and mutate document content, create versions and exports |
| Commenter | Read, presence and comment operations; no design mutations |
| Viewer | Read, presence and permitted exports |

Sensitive actions require explicit permissions even for broad roles: tenant deletion, provider-key
management, support access, audit export and ownership transfer.

## Public API

Initial route groups:

```text
POST   /v1/auth/embed/exchange
POST   /v1/auth/logout
GET    /v1/me

GET    /v1/tenants
GET    /v1/tenants/${TENANT_ID}
GET    /v1/tenants/${TENANT_ID}/usage

GET    /v1/workspaces
POST   /v1/workspaces
GET    /v1/workspaces/${WORKSPACE_ID}
PATCH  /v1/workspaces/${WORKSPACE_ID}

GET    /v1/projects
POST   /v1/projects
GET    /v1/projects/${PROJECT_ID}
PATCH  /v1/projects/${PROJECT_ID}

GET    /v1/documents
POST   /v1/documents
GET    /v1/documents/${DOCUMENT_ID}
PATCH  /v1/documents/${DOCUMENT_ID}
DELETE /v1/documents/${DOCUMENT_ID}
POST   /v1/documents/${DOCUMENT_ID}/restore
POST   /v1/documents/${DOCUMENT_ID}/realtime-ticket
POST   /v1/documents/${DOCUMENT_ID}/share-links
GET    /v1/documents/${DOCUMENT_ID}/versions
POST   /v1/documents/${DOCUMENT_ID}/exports

POST   /v1/uploads
POST   /v1/uploads/${RESERVATION_ID}/complete

POST   /v1/ai/jobs
GET    /v1/jobs/${JOB_ID}
POST   /v1/jobs/${JOB_ID}/cancel

GET    /v1/webhooks
POST   /v1/webhooks
PATCH  /v1/webhooks/${WEBHOOK_ID}
DELETE /v1/webhooks/${WEBHOOK_ID}
```

List routes use bounded page sizes and opaque cursors. Mutation retries use an `Idempotency-Key`
header. Error responses include a stable code, safe message and request identifier.

## Realtime ticket

A realtime ticket is short-lived, single-use and bound to:

- Tenant.
- Document.
- Principal or share session.
- Maximum access role.
- Expected protocol range.
- Expiry.
- Random ticket identifier.

The ticket is exchanged during WebSocket setup. The relay validates it through a signed format or a
single-use record. It never accepts a tenant or role claimed only by a client message.

## Host-product integration

Each registered host system has:

- Stable system identifier.
- Allowed token issuers and signing keys.
- Allowed audiences.
- External tenant and workspace mappings.
- Maximum delegated roles.
- Allowed redirect and embedding origins.
- Webhook endpoints and signing secrets.
- Rate and usage limits.

Recommended embedding contract:

1. Host backend creates or resolves the FigCraft document.
2. Host backend issues or requests a one-use embed token.
3. Host page loads the FigCraft URL with the token in the URL fragment or passes it through a
   constrained `postMessage` handshake.
4. FigCraft validates the parent origin and exchanges the token.
5. FigCraft sends lifecycle messages containing only registered document identifiers and status.
6. Sensitive document data remains behind normal authorized APIs.

The iframe integration must define messages for ready, dirty-state change, save acknowledged,
export requested, close requested and fatal session failure. Every message validates the exact
origin and schema.

## Webhooks

Initial event types:

```text
document.created
document.updated
document.archived
document.restored
document.deleted
document.version.created
comment.created
comment.resolved
export.completed
export.failed
ai.job.completed
ai.job.failed
member.added
member.removed
share_link.created
share_link.revoked
```

Webhook bodies include event ID, tenant ID, event type, creation time, resource identifiers and a
minimal payload. Sign `timestamp + "." + raw_body` with a per-endpoint secret. Retain delivery
attempts, cap retries and permit manual replay without changing the event identifier.

## Deletion and retention

- Archive hides a resource without deleting data.
- Soft deletion starts a configurable grace period.
- Legal or operator holds prevent object collection.
- Final deletion removes active grants, closes rooms, records the audit action and schedules object
  collection.
- Backups expire through their own documented retention schedule.
- Tenant export produces metadata, documents and referenced assets in a portable archive.
- Tenant deletion requires a second confirmation and a delayed job visible to operators.

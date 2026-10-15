# SaaS security

> **Status:** target controls · **Last updated:** 2026-10-16

## Security objectives

1. One tenant cannot read, change, enumerate or influence another tenant's resources.
2. A viewer or commenter cannot submit design mutations.
3. A confirmed document edit survives a single service-process failure.
4. Untrusted documents, assets, WASM and model outputs remain bounded.
5. Secrets do not reach browsers, plugins, logs, traces or downloadable artifacts.
6. Administrative and support access is explicit, temporary and auditable.

## Trust boundaries

| Boundary | Untrusted input |
|---|---|
| Edge | HTTP headers, cookies, tokens, WebSocket frames, origins and client addresses |
| API | Identifiers, filters, pagination cursors, idempotency keys and uploaded metadata |
| Relay | Protocol frames, change batches, presence, reconnect claims and sequence state |
| Object storage | Uploaded bytes, filenames, declared media types and checksums |
| AI | Document text, prompts, provider responses, tool calls and generated images |
| WASM | Module bytes, manifest, host-call arguments, logs and command output |
| Integration | External issuer claims, workspace mappings, webhooks and callback responses |
| Operations | Images, migrations, backups, support tools and secret delivery |

## Tenant isolation

- Resolve tenant context from validated authentication, not request body fields.
- Require tenant predicates in repository operations.
- Enable PostgreSQL row-level security on tenant-owned tables.
- Include tenant in object and cache keys.
- Authorize object downloads before signing them.
- Reject references whose child and parent tenant do not match.
- Use globally opaque identifiers to reduce enumeration signals.
- Normalize not-found and forbidden responses where revealing existence creates risk.
- Test every resource route with a valid identifier from another tenant.
- Separate operator capabilities from tenant administrator roles.

## Authentication controls

- OIDC signature, issuer, audience, nonce, state and time claims are mandatory.
- Session cookies are secure, HTTP-only and same-site according to embedding mode.
- Cross-site embedding uses an explicit origin allowlist and suitable cookie/token exchange.
- Session identifiers rotate after login and privilege changes.
- Refresh tokens, when used, are encrypted and server-side.
- Service credentials are scoped, expiring and rotatable.
- Embed tokens are short-lived, audience-bound and one-use.
- Share tokens contain at least 128 random bits and are stored only as hashes.
- Password authentication, if offered for standalone mode, uses a memory-hard password hash and
  rate-limited recovery flow.

## Authorization controls

- Deny by default.
- Check resource state, tenant state, direct and team membership state, inherited team grants and
  requested action.
- Enforce permissions in API and relay code, not only client controls.
- Recheck long-running job authorization before committing its result.
- Revoke active WebSocket sessions when their direct grant, team membership or team grant is
  revoked.
- Use a separate explicit permission for billing, provider keys, tenant export, tenant deletion and
  support access.
- Record actor, effective grant and outcome for sensitive operations.

## Collaboration controls

- Realtime tickets are short-lived and bound to one document and role.
- The relay ignores tenant, role or principal fields asserted by normal client messages.
- Message size, rate, queue bytes and connection count are bounded.
- Viewers and commenters cannot send design batches.
- Server validation prevents invalid trees, identifier collisions and oversized documents.
- Lease fencing prevents stale room owners from appending.
- Events are confirmed only after database commit.
- Presence is ephemeral and rate-limited separately from durable mutations.
- Reconnect tokens reveal no permanent share or service credential.

## Upload and object controls

- Reserve uploads before issuing a presigned request.
- Bind reservations to tenant, expected size, checksum, type and expiry.
- Upload to staging keys that cannot be referenced as verified assets.
- Decode images and archives within pixel, depth, entry and expanded-byte budgets.
- Verify checksums after upload.
- Reject path traversal and control characters in logical filenames.
- Serve downloads with safe content types and attachment policy where appropriate.
- Disable public bucket listing and anonymous administration.
- Expire abandoned staging objects.
- Keep malware scanning pluggable for enterprise policy without treating it as a replacement for
  safe parsing.

## AI controls

- Provider keys are encrypted and available only to the AI role.
- Context is minimized and authorized at job execution time.
- Document text is treated as untrusted data, not trusted system instruction.
- Provider tool calls map to a fixed allowlist and validated schemas.
- Model-generated commands pass normal engine and authorization validation.
- AI cannot write snapshots or database events directly.
- Generated assets pass normal validation and moderation.
- Usage reservation prevents unbounded spend during retries.
- Tenant retention policy controls prompt and response storage.
- Provider and regional policy can block external processing.

## WASM controls

- Run customer modules outside API and relay processes.
- No WASI, raw sockets or host filesystem access by default.
- Enforce module size, memory, fuel, deadline, call and byte budgets.
- Validate imports before storage and before execution.
- Issue capabilities for one tenant, document, module version and job.
- Accumulate mutations and submit one command batch only after successful execution.
- Treat module logs as untrusted bounded strings.
- Store immutable approved module versions by checksum.
- Revoke approval without deleting audit evidence.

## Secret management

- Production secrets come from an orchestrator secret store or dedicated secret manager.
- Docker Compose supports mounted secret files.
- Secret values never appear in configuration diagnostics.
- Use separate database, Redis, object-store, AI and webhook credentials by role where supported.
- Rotate signing keys with an overlap window identified by key ID.
- Rotate database and object credentials without rebuilding images.
- Backups containing secrets are encrypted and access-controlled.
- Developer defaults are visibly non-production and rejected when a public production mode is
  selected.

## Browser and embedding controls

- Set a restrictive Content Security Policy for the hosted editor.
- Allow framing only from registered host-product origins.
- Validate every `postMessage` origin and payload schema.
- Keep embed tokens out of referrer-visible query parameters.
- Use explicit CORS allowlists; never reflect arbitrary origins with credentials.
- Protect cookie-authenticated mutations against cross-site request forgery.
- Avoid storing durable bearer tokens in local storage.
- Apply secure WebAssembly and worker source policies without permitting arbitrary script hosts.

## Network controls

- TLS terminates at the trusted edge; internal production traffic uses private networks and TLS
  where the platform requires it.
- PostgreSQL, Redis and object-store administration are not internet-accessible.
- Only the gateway reaches public API and relay ports.
- Workers receive only the egress required by their role.
- Local AI runtimes accept requests only from the AI worker network.
- Trusted-proxy configuration names exact proxy networks.
- Cloud instance metadata endpoints are unreachable from untrusted execution networks.

## Audit events

Audit at minimum:

- Authentication success and failure category.
- Membership and role changes.
- Workspace, project and document creation, archive, restore and deletion.
- Share-link creation, use category and revocation.
- Provider configuration and key rotation without secret values.
- AI mutation, usage and policy rejection.
- WASM publication, approval, execution and revocation.
- Tenant export and deletion.
- Billing-plan and quota changes.
- Operator support access.
- Backup restoration and manual data repair.

Audit rows are append-only to application roles. Retention and export follow tenant plan and legal
requirements.

## Abuse and resource protection

- Rate-limit authentication, token exchange, WebSocket connection, join-code attempts, uploads,
  exports, webhooks, AI and WASM separately.
- Apply concurrency quotas in addition to request rates.
- Bound database queries, page sizes and sortable fields.
- Bound decompression, image pixels, document nodes, message bytes and outbound queues.
- Use tenant storage and compute quotas.
- Prefer refusal to unbounded in-memory queues.
- Keep operator block and moderation controls for public sharing.
- Prevent one tenant from exhausting shared worker pools through weighted scheduling.

## Supply chain

- Pin Rust dependencies through `Cargo.lock`.
- Review security advisories and license compatibility.
- Generate software bills of materials for release images.
- Sign release images and provenance.
- Scan base and final images.
- Keep base images minimal and versioned.
- Protect release environments and signing credentials.
- Verify downloaded migration and deployment artifacts by release provenance.

## Security testing

- Unit tests for role and permission matrices.
- PostgreSQL row-level-security integration tests.
- Cross-tenant API, object, job, webhook, cache and realtime tests.
- Property and fuzz tests for tokens, protocol frames, migrations and parsers.
- Race tests for membership revocation and active WebSockets.
- Split-brain tests for relay fencing.
- Presigned-request scope and expiry tests.
- Prompt-injection and AI tool-call tests.
- WASM escape, exhaustion and capability tests.
- Dependency and container scanning in CI.
- External penetration test before multi-tenant production launch.

## Incident response

Security incidents follow:

1. Detect and assign severity.
2. Preserve logs, traces, database evidence and affected image versions.
3. Contain credentials, tenants, routes, providers or modules as narrowly as possible.
4. Determine affected tenants, resources and time range.
5. Eradicate the cause and rotate relevant secrets.
6. Restore verified service and monitor for recurrence.
7. Notify affected parties according to policy and law.
8. Record corrective actions in the tracker and regression tests.

## Production security gate

- Tenant isolation suite passes against the release image.
- Relay fencing and durable-confirmation failure tests pass.
- No default or example secret is accepted in production mode.
- Backups restore in an isolated exercise.
- Image provenance and software bill of materials are published.
- Critical and high exploitable vulnerabilities are resolved or explicitly blocked from release.
- Operator access and audit export are tested.
- External identity, embed and webhook signature flows pass adversarial tests.
- Public terms, privacy, retention and subprocessors match actual system behavior.

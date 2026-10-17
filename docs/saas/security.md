# SaaS security

> **Status:** target controls · **Last updated:** 2026-10-17

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

## Platform realm (S6-044)

The deployment has four levels: platform, workspace (the `tenants` table), team, project. The
platform level is a separate realm, not a super-tenant.

- **Platform admins are not workspace users.** They live in `platform_admins` (own login, Argon2id
  password hash, sealed TOTP secret, lockout counters), have no `principals` row and no membership
  in any workspace, and sign in only at `/platform/v1`. Their sessions (`platform_sessions`) use a
  different cookie name and path, shorter lifetimes (15 minutes idle, 4 hours absolute) and
  separate rate limits. A workspace session cannot authenticate a platform request and a platform
  session cannot authenticate a workspace request (tested both ways).
- **Platform tables are unreachable from a workspace transaction.** `platform_admins`,
  `platform_sessions`, `platform_bootstrap` and `platform_audit_events` have no tenant column and
  carry forced row-level security that opens only while `figcraft.platform_scope` is `on` in the
  transaction. Only `begin_platform_transaction` sets it, and it never sets the tenant context, so
  a platform transaction reads zero rows of any workspace table. `platform_audit_events` is
  append-only for the runtime role (no UPDATE or DELETE grant).
- **The platform sees workspace metadata only.** Listing and reading workspaces go through two
  SECURITY DEFINER functions that refuse callers outside a platform transaction and return the
  `tenants` metadata columns plus an active-member count. Suspending, restoring and archiving run
  through the workspace's own audited lifecycle functions. The platform cannot read documents,
  files, names of members, comments or workspace audit content.
- **What this does not defend against.** One database role serves both realms; the separation is
  row-level security plus a single code path. Code that runs in the cloud process and sets the
  platform scope itself would bypass it, so the setting is written in exactly one function and
  reviewed as such. A second database role with grants only on the platform tables is the stronger
  option and is recorded as future hardening (S8).
- **Bootstrap.** The first admin is created with a one-time token printed by the CLI
  (`--print-platform-bootstrap-token`), valid for a short time, hashed at rest, spent atomically.
  After that, admins are created by existing admins.
- **Every platform action is audited** in `platform_audit_events` (actor, action, workspace,
  outcome, request id), including refused sign-ins, with no secrets in the metadata.

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

## Built-in accounts

Local accounts sit beside OIDC; a tenant chooses its allowed sign-in methods and neither bypasses the
other. The principal stays the identity (an OIDC user links through `identity_links`; one person may
hold both).

* **Storage** (`local_credentials`, forced RLS): normalised login (NFC, lowercase, 3 to 128
  characters, unique per tenant), PHC password hash with its parameters, `active`/`disabled`, failure
  counters. Nothing else about a password is stored.
* **Hashing** (S6-033): Argon2id, 46 MiB / 2 passes / 1 lane by default, configurable inside fixed
  bounds, parameters stored per hash and upgraded at the next successful sign-in. Verification does one
  full computation for unknown logins and unusable hashes (no account oracle in text or timing).
  Concurrent hashes are bounded. Passwords are NFKC-normalised; 12 to 128 characters; no composition
  rules; common-password and pattern deny-list. Decision record: `password-hashing-decision.md`.
* **Sign-up** (S6-034): only through an invitation (the login is the invited address; the token is
  locked first, so of concurrent sign-ups exactly one wins; principal, credential and memberships are
  one transaction), through the tenant's **open registration** (a tenant setting, **off by default**,
  `viewer` role, no team), or, for the first owner of a single-tenant deployment, with the printed
  bootstrap token plus a password (a self-hoster needs no external identity provider). Misuse of an
  invitation token is one fixed `404`; a password the policy refuses is explained (`error.reason =
  password_policy`); a taken login is `409`.
* **Per-tenant account settings** (`tenant_account_settings`, forced RLS, `tenant.update_policy`,
  audited): allowed sign-in methods (password, oidc; at least one on), open registration, second
  factor required. Contract: `apps/figcraft-cloud/openapi/accounts-api-v1.json`, served at
  `GET /api/v1/auth/openapi.json`.
* **Sign-in** (S6-035, `POST /auth/password/login`): same-origin only; exactly one Argon2 computation
  on every path; one fixed `401` for unknown login, wrong password, disabled credential, locked
  account and suspended member (text and timing). Three independent limits: the per-address token
  bucket of the auth router (10 burst, then 10 a minute), a per-replica backoff keyed by a hash of
  tenant and login that answers `429` with `Retry-After` for known and unknown logins alike, and the
  database lockout on existing accounts (default 5 failures, then 30 s doubling to 15 min; attempts
  while locked are neither counted nor extend the lock, so an attacker cannot hold a victim out;
  a correct password during a lock is refused with the same `401`). A success rotates the session
  (the browser's previous one is revoked), clears the counters and, when the stored hash is weaker
  than the configured parameters, replaces it. A tenant with passwords off answers `403`.
* **Never** logged, returned or audited: passwords, hashes, reset or invitation tokens.

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

### Upload validation budgets

`figcraft-cloud-storage::validation::validate` runs on staged bytes before promotion. It reads
headers and container directories only (no pixel decoding, no decompression) and refuses with a
fixed reason code (`media_mismatch`, `pixel_budget`, `compression_ratio`, `unsafe_name`, …) that
is safe to return to clients and audit records. `Budgets::DEFAULT`, which plans may only tighten:

| Budget | Default | Applies to |
|---|---|---|
| Upload size | 512 MiB | every upload |
| Pixels (width × height) | 100 MP | PNG `IHDR`, first JPEG `SOFn`, WebP `VP8 `/`VP8L`/`VP8X`, GIF logical screen |
| Width or height | 65 535 | images; zero is refused |
| Decoded bytes | 512 MiB (4 bytes a pixel, 8 at 16-bit depth) | images |
| Entries | 50 000 (32 plugin assets) | `.figcraft` zips, plugin bundles |
| Entry size | 512 MiB declared uncompressed (256 KiB manifest, 16 MiB module) | zips, plugin bundles |
| Expanded bytes | 2 GiB declared uncompressed in total | zips |
| Compression ratio | 250:1 for entries above 1 MiB | zips |
| Entry name | 255 bytes | zips (plugin asset names: 64) |

- Accepted types are `image/png`, `image/jpeg`, `image/webp`, `image/gif`,
  `application/vnd.figcraft.library` (documents and published libraries) and
  `application/vnd.figcraft.plugin`. The declared type is matched without ASCII case and must agree
  with the magic bytes; parameters are refused.
- SVG and PDF are refused: both carry scripts, external references and (SVG) XML entities, and a
  header check cannot make them safe. SVG enters through the client's importer, which converts it
  to document nodes; a sanitizing server path would need a full parser and is not offered.
- PNG `IHDR` must be the first chunk with a matching CRC and a legal depth and colour type. WebP
  must span the upload exactly and start with a key frame header. JPEG segment walks stop after
  1 024 segments; a zero height (deferred to `DNL`) is refused.
- Zips are read from the end-of-central-directory record, which must end the upload exactly
  (comment included) and follow the directory directly. Every entry must be stored or deflate,
  unencrypted and not zip64; its local header must repeat the central name and method; entries
  must be disjoint and the first must start at byte 0, which refuses overlap bombs and prepended
  polyglots. Names must be UTF-8 relative paths without control characters, `\`, `:`, empty, `.`
  or `..` segments, and unique without ASCII case. A `.figcraft` must contain `document.json`.
- Plugin bundles (`FCPLUGB1`) are uncompressed: the manifest must be UTF-8, the module must start
  with the WebAssembly version 1 header, asset names follow the plugin crate's plain-name rule and
  no bytes may follow the last asset.
- Declared sizes are claims. Readers that later decompress or decode still enforce the same
  limits while streaming, so a lying header cannot expand past them.

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
- The database administrator URL is mounted only into one-shot role-provisioning jobs. Migration
  and long-running cloud roles use separate non-superuser credentials; runtime cannot change
  schema or mutate migration history.
- Secret values never appear in configuration diagnostics.
- Webhook signing secrets are generated server-side, shown to the host once, and stored only as
  AES-256-GCM blobs under a subkey derived from `SESSION_SIGNING_KEY` (bound to tenant and
  endpoint as associated data). Without that key the webhook API answers `503`. Rotating
  `SESSION_SIGNING_KEY` makes stored webhook secrets unreadable until each endpoint's secret is
  rotated; a dedicated key with re-sealing is future work.
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

### Embedded sessions (S6-010)

An embed-token exchange opens an *embedded session*, not an ordinary browser session:

- It is bound, on its `browser_sessions` row, to the host system, tenant, document and role the
  token granted (`embed_*` columns, all set or all null). The binding cannot change and survives
  rotation.
- It lives in its own cookie, `__Host-figcraft_embed` (`Secure; HttpOnly; SameSite=None;
  Partitioned`, because the editor runs in a frame on the host's page). The ordinary session cookie
  never accepts an embedded session and the embed cookie never accepts an ordinary one, so a
  visitor's own sign-in and the frame's session never replace each other.
- Every request is held to the bound tenant and document and to three routes: read the document,
  mint a realtime ticket and fetch the collaboration invite. Members, sharing, rename, archive,
  export, other documents and every account-level route answer `403`. Writes still need the
  same-origin check (the frame page is our origin; a cross-site page cannot write with the cookie).
- The role is capped at the token's role: the member's own role (document, project, workspace
  and tenant) is lowered to it and explicit tenant permissions are dropped for the request.
  Realtime tickets and relay admission carry the capped edit decision.
- Both lifetimes are at most 8 hours; the host issues a new token to continue.
- Disabling the host, removing the document mapping or removing the principal mapping revokes the
  matching live embedded sessions in the same transaction (`revoke_reason = host_revoked`). The
  relay's ordinary session check then closes their open rooms. Re-enabling a host does not revive
  a revoked session. Removing the person from the tenant is caught by the same membership checks as
  for any session.

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

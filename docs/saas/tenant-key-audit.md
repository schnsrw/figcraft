# Tenant key audit (S4-015)

> **Status:** audited at schema 20261017000005 · **Last updated:** 2026-10-17

Every key that names stored or cached state must either include the tenant or be globally unique by
design, so that a key from one tenant can never address, collide with or reveal another tenant's
state. This page records each key family found in the cloud crates (`crates/cloud-*`,
`apps/figcraft-cloud`) and why it is safe. Re-run the audit when a key family is added; the search
used is listed at the end.

Verdicts: **T**: the key includes the tenant. **G**: globally unique by design (a random UUID or a
hash of a random secret), and the tenant is recorded beside it and checked on every use. **X**:
global on purpose (not tenant-owned state).

## Object-store keys

All object keys are built in one place, `cloud-storage/src/object_metadata.rs` (`keys::*`) plus
`exports::export_key`, never from caller-supplied paths, and the `objects` table enforces
`starts_with(object_key, 'tenants/' || tenant_id || '/')`, so a row can't point at another tenant's
prefix. The live-key unique index is `(bucket, object_key)`, which therefore includes the tenant.

| Key | Layout | Verdict |
| --- | --- | --- |
| Snapshot | `tenants/{tenant}/documents/{document}/snapshots/{seq}-{sha256}.bin` | T |
| Asset | `tenants/{tenant}/assets/{sha256}` (content addressing is per tenant: the same bytes in two tenants are two objects) | T |
| Library version | `tenants/{tenant}/libraries/{library}/{version}-{sha256}.figcraft` | T |
| Plugin version | `tenants/{tenant}/plugins/{plugin}/{version}-{sha256}.bin` (version validated) | T |
| Thumbnail | `tenants/{tenant}/thumbnails/{document}/{seq}.webp` | T |
| Export | `tenants/{tenant}/exports/{export}/{filename}` (filename reduced to one safe segment) | T |
| Upload staging | `tenants/{tenant}/staging/{reservation}`; `upload_reservations` CHECKs exactly this | T |
| Presigned URLs | Signed for one of the keys above, after a tenant-scoped row lookup | T |
| Reconciliation / GC listings | List under `tenants/{tenant}/`; in-memory maps are keyed by full object key | T |

## Database keys (rows used as keys, locks and leases)

| Key | Where | Verdict |
| --- | --- | --- |
| Tenant-owned rows (`workspaces`, `projects`, `documents`, `teams`, `libraries`, `plugins`, `exports`, `objects`, `share_links`, `service_accounts`, `team_invitations`, ...) | UUID primary key plus `UNIQUE (tenant_id, id)`; foreign keys between tenant-owned rows are composite `(tenant_id, id)` so a row can't reference another tenant's row | T |
| Memberships and grants | Primary keys start with `tenant_id` (`tenant_members`, `team_members`, `workspace_members`, `document_members`, `*_team_grants`, `member_permissions`, `library_usages`) | T |
| Event log | `document_events (tenant_id, document_id, seq)` | T |
| Thumbnails | `document_thumbnails (tenant_id, document_id, seq)` | T |
| Idempotency keys | `idempotency_keys (tenant_id, principal_id, route, key_hash)` | T |
| External mappings | `external_mappings (tenant_id, system_id, resource_kind, external_id)` | T |
| Retention holds | `retention_holds (tenant_id, id)` | T |
| Sequencer lease / fencing token | `documents.fencing_token` on the `(tenant_id, id)` row, claimed `FOR UPDATE` by tenant and id | T |
| Row locks | Every `FOR UPDATE` / `FOR UPDATE SKIP LOCKED` in `cloud-storage/src` filters on `tenant_id` (or on a tenant-prefixed object key) | T |
| Jobs | `jobs.id` random UUID, `tenant_id NOT NULL`, `UNIQUE (tenant_id, id)`; the claim index is global on purpose (one queue for all tenants, fair by priority and age); a claimed job runs in its row's tenant. `kind` (e.g. `document.export`) is a job type, not a key. `lease_owner` names a worker process, not tenant state | G |
| Share-link secrets | `share_links.token_hash` UNIQUE: SHA-256 of 256 random bits; resolution returns the row's tenant and the relay verifies the link's document matches the requested file | G |
| Browser sessions, OIDC login attempts, service-account credentials, team invitations | `token_hash` / `state_hash` / `secret_hash` UNIQUE: SHA-256 of a random secret; they resolve before a tenant is known, and the row carries the tenant (or the principal) | G |
| Advisory locks | None used (`pg_advisory*`, `hashtext` absent) | n/a |
| Identity | `principals.id`, `identity_links (issuer, subject)`, `tenants.slug` | X: identity and tenant names are global by design |

## Caches, in-process maps and Redis

| Key | Where | Verdict |
| --- | --- | --- |
| Redis | `REDIS_URL` is configured but no code reads or writes Redis yet. **Rule for the first user:** every key is prefixed `t:{tenant}:` unless it is a global rate-limit bucket keyed by an address or a random id | n/a |
| Relay hub rooms, `CloudBackend::admitted`, `CloudBackend::open` | Keyed by the hello's file id, which must parse as a document UUID (globally unique primary key); admission resolves the share link and refuses unless the link's document is that file, and records the link's tenant with the entry; every storage call then runs in that tenant | G |
| Relay `RevocationWatch` | File id to `(tenant, links)`; a file seen with a different tenant starts over rather than mixing; the sweep checks `(tenant, link)` pairs | T |
| Hub revoke requests | `(file, link id)`, only from this process's sweeper (random in-memory key, compared in constant time), re-checked against storage before acting | G |
| Control-plane config maps (`apps/figcraft-cloud/src/config.rs`) | Process settings, not tenant state | X |

## Findings

No key family was found that addresses tenant-owned state without the tenant or a globally unique
id. Two follow-ups:

- **Redis (future).** The first Redis use must follow the `t:{tenant}:` rule above; add a test in
  that change.
- **Self-hosted `figcraft-server`.** Its stores (`apps/figcraft-server`) are single-organisation and
  out of scope for tenant keys; they are not used by FigCraft Cloud.

## How the audit was run

```sh
rg -n 'keys::|tenants/\{|format!\("tenants' crates/cloud-* apps/figcraft-cloud
rg -n -i 'pg_advisory|hashtext|redis|valkey' crates/cloud-* apps/figcraft-cloud
rg -n 'FOR UPDATE|SKIP LOCKED|INSERT INTO jobs|lease' crates/cloud-storage
rg -n 'HashMap<|BTreeMap<|cache' crates/cloud-* apps/figcraft-cloud/src
rg -n 'PRIMARY KEY|UNIQUE' crates/cloud-storage/migrations
```

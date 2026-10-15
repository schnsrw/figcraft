# SaaS architecture

> **Status:** target design · **Last updated:** 2026-10-16

## Context

FigCraft currently has a local-first engine and a filesystem-backed collaboration server. The
existing server owns a collaboration hub, stores a snapshot plus an append-only log per file and
uses link tokens as access. This design is appropriate for self-hosting but one data-directory lock,
in-process room ownership and filesystem metadata prevent horizontal SaaS scaling.

The SaaS extension preserves the editor, collaboration model and wire compatibility while adding a
tenant-aware control plane and distributed room ownership.

## System topology

```text
Desktop client    React shell + WASM editor    Host product backend
      |                       |                         |
      +-----------------------+-------------------------+
                              |
                       HTTPS and WebSocket
                              |
                     Edge proxy or ingress
                              |
          +-------------------+-------------------+
          |                   |                   |
       Cloud API        Realtime relay       Static web assets
          |                   |
          +---------+---------+
                    |
          +---------+----------+----------------+
          |                    |                |
      PostgreSQL             Redis          S3-compatible store
          |                    |                |
          +--------------------+----------------+
                               |
                     Background workers
               snapshots, exports, AI, cleanup
```

## Modular cloud application

`figcraft-cloud` is one Rust application with role selection:

| Role | Responsibility | Durable writes |
|---|---|---|
| `all` | Runs API, relay scheduling and workers for a small deployment | PostgreSQL and S3-compatible storage |
| `api` | Authentication, tenancy, workspaces, documents, links, webhooks, billing and job submission | PostgreSQL |
| `relay` | WebSocket admission, authoritative rooms, event persistence, presence and reconnection | PostgreSQL; snapshots through S3-compatible storage |
| `worker` | Compaction, thumbnails, exports, object cleanup, webhook delivery, AI and server WASM | PostgreSQL and S3-compatible storage |

All roles use the same configuration parser, domain types, authorization library, metrics and
release version. Production may run separate replicas without building different binaries.

## Browser composition

The browser product deliberately combines two runtimes:

| Runtime | Owns |
|---|---|
| React and TypeScript | Authentication flow, routing, workspace and project screens, libraries, notifications, administration, HTTP API client and editor lifecycle |
| Rust compiled to WASM | Document state, commands, canvas, rendering, selection, optimistic edits, collaboration changes, reconciliation and undo |
| Rust cloud service | Authorization, WebSocket authority, PostgreSQL transactions, object storage and jobs |

Browser WASM cannot open native TCP sockets. It uses browser-provided networking through bindings.
The existing `crates/collab-net/src/web.rs` already wraps `web_sys::WebSocket`. The initial SaaS
bridge keeps that Rust client as the single owner of the collaboration connection. TypeScript
obtains a short-lived document ticket over authenticated HTTP and passes the ticket and `wss:`
endpoint when opening the editor. Do not run a competing TypeScript collaboration client.

The bridge carries opaque serialized snapshots and collaboration messages plus small lifecycle
events. TypeScript does not interpret document changes, resolve conflicts, replay commands or own
an alternate document model.

## Component boundaries

### Cloud domain

Contains identifiers, tenancy context, roles, plans, quotas, resource states and shared error
types. It has no database, HTTP, Redis, S3 or UI dependency.

### Authentication

Validates OIDC sessions, service tokens, embed-token issuers and signed share tokens. It resolves a
principal and tenant context before application handlers access a repository.

### Storage

Owns PostgreSQL repositories, transactions, migrations, S3 operations and immutable object
checksums. It exposes domain-oriented operations instead of raw SQL to API and relay code.

### Relay

Uses `figcraft-collab` for document state, validation, ordering and messages. It obtains a fenced
lease, restores one document, commits new batches and publishes ephemeral presence. It does not own
account or billing policy.

### Jobs

Claims durable PostgreSQL jobs with `FOR UPDATE SKIP LOCKED`, renews job leases, records attempts and
writes final results. Redis is not required to retain a job.

### AI

Builds authorized context, chooses an allowed provider, measures usage, validates structured
outputs and applies command batches. Provider adapters cannot access tenant repositories directly.

### Server WASM

Executes signed or tenant-approved modules in a dedicated worker with explicit capabilities. It
cannot open sockets, read host files or write a document outside the command gateway.

## Request paths

### Metadata request

1. The edge terminates TLS and forwards the request identifier.
2. Authentication validates the session or token.
3. Tenant resolution produces `TenantContext`.
4. Authorization checks the requested action and resource.
5. The repository transaction sets PostgreSQL tenant context for row-level security.
6. The handler performs the operation and records an audit event when required.
7. The response contains the request identifier.

### Collaboration connection

1. The client requests a WebSocket ticket for a document.
2. The API validates membership or a share link and returns a short-lived, single-purpose ticket.
3. The gateway routes the WebSocket request to a relay replica.
4. The relay looks up the room owner in Redis.
5. If no owner is alive, one worker acquires a lease and fencing token.
6. The owner restores the latest verified snapshot and replays later PostgreSQL events.
7. The client sends the existing protocol hello and resumes or receives a snapshot.
8. For each edit, the owner validates the batch, appends it transactionally and then broadcasts the
   confirmation.

### Direct asset upload

1. The client requests an upload reservation with size, hash and media type.
2. The API authorizes the tenant and checks quota.
3. The API returns a short-lived presigned S3 request for a staging key.
4. The client uploads the bytes.
5. The client completes the reservation.
6. A worker verifies size and checksum and promotes the object to a content-addressed key.
7. The document attaches only the verified object identifier.

### Export job

1. The client submits an idempotent export request.
2. The API records a durable job and returns its identifier.
3. A worker loads the specified document sequence, renders with FigCraft and uploads the result.
4. The worker records the object and marks the job complete.
5. The client polls the job or receives a signed webhook.
6. The API returns a short-lived download URL after authorization.

## Collaboration consistency model

### Authority

- Exactly one relay owns a document room at a time.
- Ownership is a renewable Redis lease with a PostgreSQL fencing token.
- The fencing token increases whenever ownership changes.
- Every event row carries the fencing token used to append it.
- PostgreSQL rejects an append from a token older than the document's current token.

### Commit and broadcast

The relay follows this order:

1. Validate the batch against the in-memory collaboration server.
2. Prepare the resulting journal records.
3. Start a PostgreSQL transaction.
4. Lock the document sequence row.
5. Verify lease identity and fencing token.
6. Insert consecutive event rows and update the current sequence.
7. Commit.
8. Send confirmations and broadcasts.

If the commit fails, the room is closed without confirming the batch. Clients reconnect and
reconcile from durable state, matching the current server's durability rule.

### Snapshot compaction

- Snapshot jobs target a specific document sequence.
- The job restores the previous snapshot and replays events through the target sequence.
- The encoded snapshot is uploaded under an immutable object key with checksum metadata.
- The worker verifies the uploaded object.
- A PostgreSQL transaction updates the active snapshot pointer only if the document still has at
  least the target sequence.
- Events remain until the configured recovery and audit retention permits deletion.
- Old snapshots are removed only by a later garbage-collection job.

### Room recovery

A new owner loads the active snapshot, verifies its checksum, replays subsequent events and restores
the collaboration server's client-id and sequence state. Failure to verify a snapshot falls back to
the preceding retained snapshot. If no verified chain exists, the room is quarantined for operator
repair instead of serving uncertain state.

## Scaling model

### API

API replicas are stateless apart from bounded in-process caches. Sessions, idempotency results and
durable jobs live outside a replica.

### Relay

Relay replicas scale by document room. The first release does not split one document across
workers. A room may have many client connections, but all mutations pass through one authority.

### Workers

Workers claim independent durable jobs. Separate queues or concurrency pools protect interactive
thumbnails and webhooks from expensive exports and AI calls.

### Database

Use one primary PostgreSQL cluster initially. Read replicas may serve analytics and operator views,
but authorization-sensitive reads should use the primary until replica-lag behavior is explicitly
designed.

### Object storage

The local Compose stack uses single-node SeaweedFS for development and private evaluation. Hosted
production uses a separately reviewed distributed S3-compatible service or managed S3 with
versioning, lifecycle rules, network isolation and restore-tested backups.

## Protocol evolution

- Preserve the existing binary collaboration messages where possible.
- Add capabilities through negotiated protocol versions.
- Existing `figcraft-server` links remain supported by desktop and browser clients.
- SaaS admission uses a short-lived ticket that resolves to the existing document/access hello.
- Large images and future snapshots move through an asset channel rather than being repeated in
  collaboration batches.
- Clients receive explicit minimum and maximum compatible protocol versions.
- Deploy relay changes before clients depend on a new optional capability.

## Failure behavior

| Failure | Required behavior |
|---|---|
| API replica stops | Requests retry against another replica; no durable state is local |
| Relay stops before commit | No confirmation was sent; clients reconnect and resubmit through reconciliation |
| Relay stops after commit | Event is replayed by the next owner; clients learn its accepted sequence |
| Redis unavailable | New ownership pauses; current rooms may continue only while fenced database writes remain valid |
| PostgreSQL unavailable | Mutations and control-plane writes stop; no edit is confirmed |
| Object store unavailable | Existing warm rooms may edit while event limits permit; uploads, snapshots and exports pause |
| Worker stops | Its expired job lease permits another worker to retry idempotently |
| Webhook receiver unavailable | Delivery retries with backoff and preserves a visible failure state |
| AI provider unavailable | AI job fails or routes to an allowed fallback; document editing remains available |

## Dependency direction

Cloud crates may depend on existing engine and collaboration crates only at defined adapter points.
Existing low-level FigCraft crates must not depend on cloud crates. The intended direction is:

```text
cloud API / relay / worker
        |
cloud domain, auth, storage, AI and WASM
        |
existing engine, collab, format and render crates
        |
existing document, geometry and color crates
```

This preserves the current `cargo xtask layers` architecture and limits upstream conflicts.

## Multi-region boundary

The first multi-region design assigns each tenant or document a home region. Web requests may enter
through any edge, but realtime traffic routes to the home region. PostgreSQL metadata and S3
objects may replicate for disaster recovery. Active-active mutation of one document in several
regions is outside the initial architecture because it would replace the single-authority ordering
model.

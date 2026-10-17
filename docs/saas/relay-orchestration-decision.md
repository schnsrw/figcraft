# Relay orchestration decision (S5)

**Status:** Proposed. Waiting for the product owner. **Date:** 2026-10-17. **Scope:** S5-003, S5-004,
S5-006 and S5-012 in [`tracker.md`](tracker.md). This doc compares how room ownership could be
orchestrated across relay replicas. It does not reopen the consistency model in
[`architecture.md`](architecture.md#collaboration-consistency-model).

## The question

Exactly one relay may own a document room at a time. S5 needs four things around the
already-implemented database fence:

- a room registry (S5-003);
- renewable leases with an owner identity (S5-004);
- a way to route a client to the owner (S5-006);
- graceful drain for rolling releases (S5-012).

We can build these ourselves on the Redis that is already in the Compose stack (option A). Or we
can adopt an open-source session-backend orchestrator: Plane by Jamsocket (option B) or Rivet
(option C).

## Non-negotiable in every option: the PostgreSQL fence

`figcraft_cloud_storage::events::claim_sequencer` increments `documents.fencing_token` under a
`FOR UPDATE` row lock. `append_events` re-reads that token under the same lock and returns
`StaleFence` for any token that is not the current one
([`crates/cloud-storage/src/events.rs`](../../crates/cloud-storage/src/events.rs)). The relay
confirms and broadcasts only after the append commits (commit before broadcast).

A lease cannot stop a stalled process from waking up and writing after its lease has expired.
This applies to a Redis lease, a Plane key and a Rivet actor placement alike: think of GC pauses,
`SIGSTOP`, VM freezes, or a network partition healing late. Only a fence checked by the store
itself can stop that write ([Kleppmann, "How to do distributed locking"][kleppmann]).

So whichever orchestrator decides *who should* own a room, the new owner must call
`claim_sequencer` before it serves the room. An old owner's writes must keep failing with
`StaleFence`, and that error must close the room without confirming the batch (S5-010). No option
below replaces this layer. The orchestrator affects only availability and routing, not safety.

## Option A: our own design (Redis registry, leases, fencing, routing, drain)

What the tracker and architecture already describe:

- **Registry and lease (S5-003/004).** One namespaced key per room, for example
  `figcraft:{deployment}:room:{tenant}:{document}`. Its value is `{relay_id, epoch, endpoint}`
  with a bounded TTL. Acquiring uses `SET NX PX`. Renewing and releasing use compare-and-set
  Lua scripts keyed on `relay_id`. The total number of keys is bounded by open rooms, and every
  value has a size cap. Redis is already a digest-pinned service in
  [`deploy/saas/compose.yaml`](../../deploy/saas/compose.yaml).
- **Fence (S5-005).** This is done. A successful lease acquisition is followed by
  `claim_sequencer`, and the room serves only with the token that call returns.
- **Routing (S5-006).** The API that issues the realtime ticket looks up the owner, or picks one
  and acquires the lease, then puts the owner's endpoint in the ticket response. Any relay that
  receives a connection for a room it does not own replies with a "moved" close carrying the
  owner, and the client reconnects. When there is only one relay (the single-tenant Compose
  default), this collapses to "every room is local", and the gateway route stays as it is
  (`/ws/v1/collab` to `cloud:8082`). Caddy can also pin rooms to replicas by hashing a document
  query parameter (`lb_policy query` or `uri_hash` [caddy-rp]). That works as an optimisation,
  but it is not authority, because hash placement changes when the replica set changes.
- **Drain (S5-012).** On `SIGTERM`, a relay stops renewing and stops accepting rooms. For each
  room it flushes, snapshots, releases the lease and sends a reconnect signal. Clients reconnect,
  the next relay acquires the lease and runs `claim_sequencer`, then recovers through the
  existing DurableRoom snapshot-plus-replay path.

**Strengths.** No new services or images. The relay stays a single multi-document hub process,
using the same `collab-net` hub as `figcraft-server`, which keeps S5-019 compatibility simple.
Admission (share links, realtime tickets, RLS tenant transactions) is unchanged. It works from one
`docker compose up`.

**Costs.** We write and test the lease and redirect logic ourselves: roughly S5-003/004/006/012
plus the S5-020/021 tests. Lease correctness under Redis failover is weaker than a consensus
store's. That is acceptable only because the PostgreSQL fence is the safety layer.

## Option B: Plane (jamsocket/plane)

**Facts (checked 2026-10-17):**

- **Licence.** MIT, "Copyright (c) 2023 Drifting in Space Corp.". The LICENSE file also contains
  MIT-licensed code from `hyperium/hyper-util` ([LICENSE][plane-license]).
- **Activity.** The last release was `v0.5.5` on 2025-02-10 ([releases][plane-releases]). The last
  commit on `main` was 2026-01-13, from a run of operational fixes in January 2026
  ([commits][plane-commits]). The repo has about 2.0k stars and 4 open issues, and it is not
  archived.
- **Ownership.** Jamsocket joined Modal Labs on 2025-07-10. The announcement says Jamsocket
  products "will continue to operate normally while the core functionality is being migrated to
  Modal". It makes no stated commitment about Plane's roadmap or maintenance
  ([Modal blog][modal]). In practice the project looks like it is in maintenance mode: no release
  in 20 months, and no commits for about 9 months.
- **Architecture.** The **controller** is the hub and "the only component that holds a direct
  connection to the database". **Drones** are machines running `plane drone`, "issuing Docker API
  calls locally on behalf of the controller". **Proxies** route inbound traffic to the right
  backend and terminate TLS. An optional **DNS server** answers ACME DNS-01 challenges
  ([architecture][plane-arch]).
- **Database.** Plane requires its own **PostgreSQL**: "The source-of-truth for all persisted
  information in Plane is a Postgres database. The database also acts as a real-time broadcast
  message bus between controller instances" ([deploy to prod][plane-prod]).
- **Packaging.** A session backend is a **Docker image per backend**. `spawn_config.executable.image`
  names it ([API][plane-api]). The drone sets `PORT=8080`, `SESSION_BACKEND_ID`, `SESSION_BACKEND_KEY`,
  `SESSION_BACKEND_FENCING_TOKEN` and, optionally, `SESSION_BACKEND_STATIC_TOKEN`
  ([docker runtime][plane-docker]). The tree also has an experimental `unix_socket` runtime that
  is marked `#[allow(unused)]`. The drone needs `/var/run/docker.sock`, as its own Compose file
  shows ([compose][plane-compose]). Drones are "not compatible with being run in Kubernetes"
  ([deploy to prod][plane-prod]).
- **Single owner.** "For a given key, Plane guarantees that at most one backend will be running at
  a time." The controller acquires the key on `connect`, and the drone renews it. If renewal
  fails, the drone soft-terminates and then hard-terminates the backend by deadlines measured on
  its own clock. `allow_renew = false` forces termination when a drone is lost. Each key has a
  fencing token, taken from the database's epoch milliseconds at acquisition, which is "provided
  to the backend itself for optional use" ([keys][plane-keys]). A backend whose drone is
  permanently lost never reaches `terminated` ([lifecycle][plane-lifecycle]).
- **Auth and routing.** Your stateless backend authorises the user, calls `/ctrl/connect`, and
  gets back a tokenised URL. The token is a bearer token in the path. `user` and `auth` reach the
  backend as `x-verified-username` and `x-verified-user-data` headers. An optional `secret_token`
  can be checked as a second factor ([auth][plane-auth]). The `/ctrl/*` API must not be public.
  Clusters named with a port run plain HTTP, which is "not intended for production use", and
  HTTPS on a non-standard port is not possible ([clusters][plane-clusters]).
- **Cold start.** A backend goes through scheduled, loading (image pull), starting (container
  start), waiting (port listening) and then ready ([lifecycle][plane-lifecycle]). Plane publishes
  no latency figure. Each document open would pay for a container start, then our own
  snapshot-plus-replay recovery.

**What this means for FigCraft:**

- **Process per document vs our hub.** Plane's unit is one container per key. With key =
  document, every open document gets its own process, PostgreSQL pool, S3 client and memory
  baseline. Our relay is designed as one hub that hosts many rooms and shares those resources.
  We could key Plane by a shard (key = `shard-N`, each backend our multi-room hub), but then
  Plane is only placing N long-lived containers. That is roughly the work our lease does, plus a
  controller, a proxy and a second database.
- **Failure semantics.** Plane's single-owner guarantee is a lease with self-termination enforced
  by the drone. That is the same class of guarantee as our Redis lease: a frozen drone can outlive
  its deadline. Plane says so implicitly by handing out a fencing token "for optional use". We
  would still call `claim_sequencer` on backend start, so Plane's token adds nothing on top of
  ours.
- **Admission.** The cloud API would call `/ctrl/connect` after share-link or member admission.
  It would send `user = principal` and `auth = {tenant, role, ticket}`. The relay would then trust
  `x-verified-user-data` instead of redeeming the realtime ticket itself. Revocation (S5-015)
  stays our sweeper's job. Caddy would front the Plane proxy in HTTP-cluster mode, which Plane
  documents as development-only, or Plane would terminate TLS itself, which means a second TLS
  edge alongside Caddy.
- **Self-hosted footprint.** A single-tenant Compose stack would gain a controller, a drone with
  the host Docker socket mounted, a proxy and a Plane database. Mounting the Docker socket is
  root-equivalent on the host, a large new trust boundary in a customer's deployment. Each of
  these images would also need a D-001 digest-pinning review.
- **Licensing.** MIT is compatible with distributing FigCraft as MIT OR Apache-2.0. We would need
  to carry Plane's and hyper-util's notices.

## Option C: Rivet (rivet-dev/rivet)

**Facts (checked 2026-10-17):**

- **Licence.** Apache-2.0 ([LICENSE][rivet-license]).
- **Activity.** Very active. `v2.3.27` was released on 2026-10-12, with several releases a week,
  and commits on 2026-10-13 ([releases][rivet-releases]). The repo has about 6.3k stars.
- **Model.** Rivet "schedules Actors: long-lived processes with durable state … each addressed by
  key". "An Actor lives inside a worker process you already run, not in its own container or VM."
  Actors hibernate when idle and are rescheduled elsewhere when a worker is lost
  ([README][rivet-readme]). The engine is one Rust control-plane binary with internal packages
  for the gateway (`guard`), scheduling (`pegboard`), workflows (`gasoline`) and storage
  (`universaldb`). The worker SDK `rivetkit` exists for TypeScript and Rust. The Rust SDK is
  "in beta" ([Rust quickstart][rivet-rust]). Raw WebSocket handlers are supported
  ([WebSocket handler][rivet-ws]).
- **Database.** Self-hosted storage is the file system ("recommended for single-node"), PostgreSQL
  ("multi-node … light-to-moderate workloads") or FoundationDB ("requires enterprise")
  ([self-host][rivet-selfhost]).
- **Cold start.** The project claims 12.3 ms p50 and 27.7 ms p99 to wake an actor from
  hibernation. That was measured on FoundationDB with the Rust SDK and "an experimental Rivet
  feature" ([README][rivet-readme]). It is not representative of the file-system or PostgreSQL
  backends a self-hosted customer would run, and it excludes our snapshot-plus-replay recovery.
- **Failure semantics.** All connections for a key reach one actor instance, and a crashed actor
  is rescheduled ([lifecycle][rivet-lifecycle]). We did not find a documented fencing token that
  external writes could check. Rivet's internal fencing protects its own registration and storage
  keys, not our PostgreSQL tables. Our `claim_sequencer` stays mandatory.
- **Drain.** Version upgrades migrate actors: they go through a graceful shutdown, then wake on a
  new machine. The grace period is 15 s by default, with a 30-minute hard limit
  ([lifecycle][rivet-lifecycle]).

**What this means for FigCraft:**

- **Fit.** Rivet fits our model of one process hosting many rooms better than Plane does: rooms
  become actors inside our relay process. But the relay would have to adopt the rivetkit runtime
  and route sockets through Rivet's gateway in place of `collab-net`'s `Hosting`. That forks the
  relay transport from `figcraft-server` and puts S5-019 compatibility at risk.
- **Duplicate durability.** Rivet's per-actor SQLite or filesystem state, tiered to S3, would sit
  next to our PostgreSQL event log and S3 snapshots. We would have to ignore it so there is one
  source of truth.
- **Product direction.** The project is now positioned for "agentic workloads", releases at a high
  rate, and its Rust API is in beta. Expect churn.
- **Footprint.** Single-node self-hosting is one engine container plus its file-system volume,
  which is acceptable in one Compose file. Multi-node needs PostgreSQL, a separate engine database
  in our cluster. FoundationDB scale is an enterprise feature.
- **Licensing.** Apache-2.0 is compatible with MIT OR Apache-2.0. We would carry its NOTICE.

## Comparison

| | A: own (Redis + PG fence) | B: Plane | C: Rivet |
|---|---|---|---|
| Licence | ours (MIT OR Apache-2.0) | MIT | Apache-2.0 |
| Maintenance | ours | last release 2025-02, last commit 2026-01; owner joined Modal | very active, fast-moving |
| Unit of ownership | room in a multi-room hub | container per key | actor in our worker process |
| New services in Compose | none (Redis exists) | controller, drone (+ Docker socket), proxy, PG database | engine (+ volume, or PG database for multi-node) |
| Single-owner mechanism | Redis lease, then PG fence | PG-backed key lease plus drone self-termination; optional token | engine placement; no external fence documented |
| Fence still required | yes (implemented) | yes | yes |
| Admission | unchanged (tickets, share links) | API calls `/ctrl/connect`; relay trusts `x-verified-*` | rivetkit connection auth plus our tickets |
| Gateway | Caddy unchanged | Plane proxy behind Caddy (HTTP mode is dev-only) or a second TLS edge | Rivet gateway behind Caddy |
| `figcraft-server` parity (S5-019) | kept (same `collab-net` hub) | kept inside the container; extra layers outside | at risk (rivetkit replaces the transport) |
| Cold start | room recovery only | container start plus room recovery | claimed ms-scale plus room recovery |

## Recommendation

**Adopt option A.** Keep the PostgreSQL fence as the safety layer, as every option already
requires.

- The safety half (S5-005) is already built and tested. What remains is availability machinery,
  and a small amount of it, on Redis we already ship.
- Plane adds four components, a second database and a Docker-socket trust boundary to a
  single-tenant Compose product. Its process-per-document model fights our multi-room hub, and
  it appears to be in maintenance mode since its company was acquired. Its single-owner guarantee
  is still a lease, so it would not let us remove anything.
- Rivet is healthy and fits better, but adopting it means replacing our transport, accepting a
  beta Rust SDK and a fast-moving agent-focused platform, and ignoring its built-in durability.
  The cost is high for a problem that a lease plus redirect solves.

We should borrow Plane's ideas without its runtime:

- Acquire the lease at connect time (in the ticket issuer), not when the relay starts.
- Express lease expiry relative to the holder's own clock, with soft and hard deadlines after
  which the relay closes its own rooms.
- Have an `allow_renew = false` style flag to force a drain.
- Shut rooms down on idle (S5-011).

## Decision needed from the product owner

1. **Accept option A** (recommended). The S5 rows below proceed as written.
2. **Or choose B or C instead.** Either one would need a dependency-image review (D-001),
   acceptance of the extra services in the self-hosted Compose stack, and, for B, acceptance of
   mounting the Docker socket and of relying on a project with no recent release.

Either way, also confirm that **one relay replica stays the single-tenant Compose default**, with
multi-replica relays only in hosted or large deployments.

## Effect on the S5 rows

| Row | A: own | B: Plane | C: Rivet |
|---|---|---|---|
| S5-003 registry | Redis keys `figcraft:{deployment}:room:{tenant}:{document}`, with bounded TTL and value size | replaced by Plane's key table in Plane's PostgreSQL; we keep only a document-to-key mapping | replaced by Rivet actor keys; the registry is internal to the engine |
| S5-004 leases | `SET NX PX` + compare-and-set renew and release; soft and hard local deadlines close rooms | replaced by Plane's key acquisition and renewal; the backend still calls `claim_sequencer` on start | replaced by actor placement; `claim_sequencer` in `onWake` before serving |
| S5-006 routing | ticket carries an owner hint; relay "moved" close and client reconnect; single replica = local | API calls `/ctrl/connect`; client connects to the tokenised Plane proxy URL behind Caddy | client connects through Rivet's gateway by actor key; Caddy forwards to the engine |
| S5-012 drain | `SIGTERM`: stop renewing, then flush, snapshot, release, reconnect signal; next owner fences | soft-terminate (`SIGTERM`, 10 s grace) per backend; our drain logic runs in each container | Rivet version migration (15 s default grace); our flush and snapshot in `onSleep` |

In every column, S5-005 (fence), S5-009 (commit before broadcast), S5-010 (reject after lease or
fence loss) and S5-021 (stale-owner fencing test) stay as specified.

## Sources

[kleppmann]: https://martin.kleppmann.com/2016/02/08/how-to-do-distributed-locking.html
[caddy-rp]: https://caddyserver.com/docs/caddyfile/directives/reverse_proxy
[plane-license]: https://github.com/jamsocket/plane/blob/main/LICENSE
[plane-releases]: https://github.com/jamsocket/plane/releases
[plane-commits]: https://github.com/jamsocket/plane/commits/main
[modal]: https://modal.com/blog/jamsocket-is-joining-modal
[plane-arch]: https://github.com/jamsocket/plane/blob/main/docs/pages/concepts/architecture.mdx
[plane-prod]: https://github.com/jamsocket/plane/blob/main/docs/pages/deploy-to-prod.mdx
[plane-api]: https://github.com/jamsocket/plane/blob/main/docs/pages/plane-api.mdx
[plane-docker]: https://github.com/jamsocket/plane/blob/main/plane/src/drone/runtime/docker/commands.rs
[plane-compose]: https://github.com/jamsocket/plane/blob/main/docker/docker-compose.yml
[plane-keys]: https://github.com/jamsocket/plane/blob/main/docs/pages/developing/keys.mdx
[plane-lifecycle]: https://github.com/jamsocket/plane/blob/main/docs/pages/concepts/backend-lifecycle.mdx
[plane-auth]: https://github.com/jamsocket/plane/blob/main/docs/pages/concepts/auth.mdx
[plane-clusters]: https://github.com/jamsocket/plane/blob/main/docs/pages/concepts/clusters.mdx
[rivet-license]: https://github.com/rivet-dev/rivet/blob/main/LICENSE
[rivet-releases]: https://github.com/rivet-dev/rivet/releases
[rivet-readme]: https://github.com/rivet-dev/rivet
[rivet-rust]: https://www.rivet.dev/actors/docs/quickstart/rust
[rivet-ws]: https://rivet.dev/docs/actors/websocket-handler/
[rivet-selfhost]: https://www.rivet.dev/docs/deploy/self-host/control-plane/
[rivet-lifecycle]: https://www.rivet.dev/docs/actors/lifecycle

- Kleppmann, distributed locking and fencing tokens: <https://martin.kleppmann.com/2016/02/08/how-to-do-distributed-locking.html>
- Caddy `reverse_proxy` load balancing: <https://caddyserver.com/docs/caddyfile/directives/reverse_proxy>
- Plane repository, licence, releases, commits: <https://github.com/jamsocket/plane>,
  <https://github.com/jamsocket/plane/blob/main/LICENSE>, <https://github.com/jamsocket/plane/releases>,
  <https://github.com/jamsocket/plane/commits/main>
- Plane docs (in the repository): architecture, deploy to prod, API, keys, backend lifecycle,
  auth, clusters, under <https://github.com/jamsocket/plane/tree/main/docs/pages>
- Plane Docker runtime env vars: <https://github.com/jamsocket/plane/blob/main/plane/src/drone/runtime/docker/commands.rs>
- Jamsocket joins Modal (2025-07-10): <https://modal.com/blog/jamsocket-is-joining-modal>
- Rivet repository, licence, releases: <https://github.com/rivet-dev/rivet>,
  <https://github.com/rivet-dev/rivet/blob/main/LICENSE>, <https://github.com/rivet-dev/rivet/releases>
- Rivet self-host storage: <https://www.rivet.dev/docs/deploy/self-host/control-plane/>
- Rivet Rust quickstart: <https://www.rivet.dev/actors/docs/quickstart/rust>
- Rivet actor lifecycle: <https://www.rivet.dev/docs/actors/lifecycle>
- Rivet WebSocket handler: <https://rivet.dev/docs/actors/websocket-handler/>

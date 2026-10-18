# Enterprise PRD/HLD/LLD review

> **Status:** reconciled with the repository · **Reviewed:** 2026-10-16 · **Input:**
> `FigCraft_Enterprise_PRD_HLD_LLD_Architecture_v1.docx`

> **Later browser amendment:** the 2026-10-18 owner decision
> [SAAS-ADR-018](wasm-first-browser-decision.md) supersedes this review's React-owned browser
> composition. The rest of this file records the 2026-10-16 proposal review as it occurred.

## Verdict

The supplied proposal has the right product and distributed-systems direction. Its three-layer
model is adopted:

1. The existing FigCraft engine remains the source of document, rendering and command behavior.
2. A collaborative document platform adds authentication, sequencing, persistence and recovery.
3. A SaaS product layer adds workspaces, libraries, review, integration and administration.

The proposal is not an implementation specification without correction. Its React/TypeScript
browser shell needs an explicit, narrow exception to this repository's current Rust-only rule,
several proposed wire types duplicate an existing protocol, and a permanent `upstream-sync` branch
would make upstream integration harder to audit. The corrected decisions below are authoritative
for this fork.

## Source claims verified locally

| Proposal claim | Repository evidence | Disposition |
|---|---|---|
| The document uses a persistent ordered model | [`../architecture.md`](../architecture.md) describes `figcraft-doc` as an `imbl` persistent map with stable identifiers | accurate |
| Editing is command-driven | [`../architecture.md`](../architecture.md) and the root agent rules require all user-visible behavior to use engine commands | accurate and preserved |
| Browser and desktop share the editor | `apps/figcraft-web/src/main.rs` runs the same `figcraft_ui_egui::FigApp` through eframe/WASM | accurate and preserved |
| Rendering is client-side | `figcraft-render` uses tiny-skia and feeds desktop, browser and export paths | accurate and preserved |
| Collaboration already has ordering, reconciliation, reconnect and per-user undo | [`../collab-protocol.md`](../collab-protocol.md) and `crates/collab` document and test these behaviors | accurate; extend instead of replacing |
| The standalone server has a persistence seam | `figcraft-collab-net::hub::Backend` admits, opens, journals and flushes rooms before replies are released | accurate; this is the first adapter seam to investigate |
| Current readiness is approximately 53% overall and 13% collaboration | Root [`../../ROADMAP.md`](../../ROADMAP.md) reports those maintainer estimates on 2026-10-14 | accurate as estimates, not production benchmarks |

The repository contains [`../collab-protocol.md`](../collab-protocol.md), not a file named
`collaboration-protocol.md`. Tickets and reviews must use actual paths from the pinned fork commit.

## Decision disposition

| Supplied proposal | Decision for this fork | Reason |
|---|---|---|
| React + TypeScript + Vite SaaS shell | adopt only for a fork-owned `webapp/` workspace shell and typed browser bridge after the root policy is amended | React fits account, workspace and administration UX; it must not duplicate editor or collaboration semantics |
| Separate `saas-api`, `saas-collab` and `saas-worker` binaries immediately | start with one `figcraft-cloud` binary exposing `all`, `api`, `relay` and `worker` roles | Logical boundaries remain clear while a solo deployment ships one image |
| New JSON collaboration operation model | preserve the existing binary `figcraft-collab` message and change representation, adding negotiated admission capabilities | A second diff model would create conversion bugs and incompatible histories |
| PostgreSQL ordered operation log | adopt | Transactions, idempotency and fencing provide the durable source of truth |
| Redis locks and presence | use Redis only for ephemeral routing, leases, presence, cache and limits | PostgreSQL must reject stale-owner writes with a monotonic fencing token |
| S3 or MinIO document and asset storage | adopt for immutable snapshots, assets, exports, libraries and plugin bundles | Large immutable objects do not belong in database rows or shared filesystems |
| NATS JetStream | defer behind a durable job/outbox abstraction | It is not required for initial sequencing and adds an operator dependency |
| Docker Compose, Swarm and Kubernetes | Compose first; add a Swarm-compatible stack for an identified deployment; add Kubernetes after behavior is stable | Packaging follows demonstrated operational need |
| OIDC and SAML | OIDC first; federate SAML through an identity broker; add native SAML only if a customer requirement cannot be met by the broker | Avoid maintaining a second authentication protocol prematurely |
| SCIM 2.0 | enterprise milestone | Provisioning depends on a stable membership and audit model |
| `figcraft-server` as legacy sharing | retain it as the supported filesystem-backed small deployment | Existing users and upstream behavior must keep working |
| Permanent `upstream-sync` branch | use a short-lived `upstream-sync/YYYY-MM-DD` branch per integration | Each merge records an exact upstream boundary without accumulating unrelated work |
| TypeScript editor bridge and generated client packages | adopt under `webapp/packages`; keep the Rust side narrow, versioned and tested | Browser types provide lifecycle safety while Rust owns document state and wire semantics |
| One file identity on browser and desktop | adopt | A cloud document must not fork merely because a different client opened it |

## Product requirements adopted from the proposal

The following supplied requirements were missing or not explicit enough in the first SaaS draft and
are now part of the product requirements and tracker:

- Browser navigation for home, recents, projects, libraries, search, notifications and settings.
- A stable workspace/document deep link and one cloud document identity across client types.
- A typed TypeScript-to-Rust/WASM host lifecycle for open, close, local changes, remote changes,
  connection state and export.
- Desktop cloud browsing with visible `saved`, `saving`, `offline`, `conflict` and `error` states.
- A durable offline queue whose unsafe conflicts become recoverable branches rather than discarded
  edits.
- Immutable library publications, explicit subscriber updates and preserved instance overrides.
- Version restore as a new head so historical revisions remain immutable.
- Enterprise federation and SCIM as post-core identity work.
- Browser accessibility, multi-client convergence and 1k/10k/20k-node performance fixtures.

## Important implementation corrections

### Preserve the current collaboration semantics

The current protocol sends resulting state changes, not replayed engine commands. SaaS admission
adds identity, tenant, document and role context around that protocol. It does not translate edits
to the illustrative `property.set` JSON from the proposal.

The current hub already withholds replies until `Backend::flush` has made journal changes durable.
The PostgreSQL backend must preserve this semantic while adding idempotent operation identifiers,
transactional sequence assignment and a fencing check. Any asynchronous adapter must prove that a
failed commit closes the room before clients receive confirmation.

### Avoid creating two canonical histories

Named versions, collaboration snapshots and the event journal serve different purposes:

- The event journal is the authoritative accepted sequence.
- Compaction snapshots accelerate recovery at an exact sequence.
- Named versions are user-visible checkpoints that point to or materialize an exact sequence.
- Restoring a version creates a new accepted head; it never rewrites the old journal.
- Offline conflict branches preserve unmergeable user work without becoming an invisible second
  head.

### Keep the browser editor Rust-authoritative

The React shell owns routing, account and workspace screens, administration and the outer editor
lifecycle. The Rust/WASM editor continues to own the canvas, document, commands, local optimistic
state, rendering and collaboration changes. TypeScript must not implement a second document model,
conflict resolver, command registry or collaboration protocol.

The current root agent policy prohibits handwritten JavaScript and TypeScript. Before browser-shell
implementation begins, the fork maintainer must amend that rule with a directory-scoped exception
for `webapp/`, define Node/package-manager and lockfile policy, add typecheck/lint/test/build and
license gates, and keep upstream-owned Rust paths unchanged. Until that governance change lands,
the hybrid shell is an approved architecture decision but not an allowed code change.

### Separate targets from current capability

The proposal's 16 ms local edit, 150 ms remote visibility, 20-editor and 20,000-node figures are
useful test profiles, not verified capability. Before they become release gates, record reference
hardware, browser, document fixture, operation mix, warm-up, sample size, percentile method and
network conditions. Server processing and end-to-end visibility remain separate metrics.

## Repository-specific implementation seams

| Concern | First source location to audit | Intended change style |
|---|---|---|
| Collaboration state and validation | `crates/collab/src/server.rs`, `replica.rs`, `message.rs` | preserve types; extend negotiated capabilities and tests |
| Durable room persistence | `crates/collab-net/src/hub.rs` `Backend` | add a narrow cloud adapter or evolve the trait without cloud dependencies below L6 |
| Filesystem migration | `apps/figcraft-server/src/store.rs` | read existing snapshots, journals, links, libraries and plugins through an idempotent importer |
| Browser lifecycle | `apps/figcraft-web/src/main.rs`, `web.rs` plus new `webapp/` | narrow Rust/WASM exports consumed by the TypeScript lifecycle bridge |
| Desktop cloud lifecycle | `apps/figcraft` and engine session boundaries | local/cloud repository adapter with explicit sync state |
| Command authorization | `crates/engine/src/cmd`, `figcraft-mcp` | keep command IDs; add tenant-scoped authorization before remote execution |
| Snapshot encoding | `crates/format` | reuse format versions and add compatibility fixtures before changing storage envelopes |
| Team libraries | current server library protocol and format modules | keep asset identity and overrides; move immutable payloads to object storage |

## Acceptance additions

In addition to the release gates in this directory, implementation must demonstrate:

- A viewer cannot submit a mutation through HTTP, WebSocket, MCP, AI or a stale offline queue.
- Retrying an accepted operation after a lost acknowledgement returns its original revision.
- A stale relay fencing token cannot append even when the stale process is still alive.
- Browser and desktop open the same cloud identifier and converge on the same accepted sequence.
- An offline conflict leaves a recoverable branch and never silently drops local work.
- A library update keeps explicit instance overrides.
- A corrupted snapshot falls back to a verified chain or fails closed.
- Performance results state the fixture and environment rather than presenting planning targets as
  measured facts.

## Conclusion

The supplied enterprise document makes sense after these corrections. Its strongest contributions
are the product journeys, cross-platform document identity, offline conflict handling, library
versioning, explicit conflict cases and measurable load profiles. The authoritative implementation
plan remains the documents in this directory because they also define the fork's narrow TypeScript
exception, three deployment modes, AI and server-WASM isolation, MinIO object lifecycle,
multi-tenant row security, embedding contracts and upstream synchronization.

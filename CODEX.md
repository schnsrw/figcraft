# FigCraft fork handoff

> **Purpose:** durable session context for Codex and other repository agents  
> **Last updated:** 2026-10-16  
> **Active program:** self-hostable single-tenant, multi-tenant and embedded FigCraft SaaS

This file is the short operational handoff. The detailed and authoritative scope is
[`docs/saas/tracker.md`](docs/saas/tracker.md); architecture is indexed from
[`docs/saas/README.md`](docs/saas/README.md). Update this handoff before ending a session whenever
the continuation point, a blocker or a major decision changes.

## Session startup

1. Run `git status -sb` before changing anything. Preserve user changes.
2. If the tree is clean, run `git pull --ff-only origin main`. If it is not clean, inspect and work
   around the changes; never discard them to make a pull succeed.
3. Read `AGENTS.md`, this file, `ROADMAP.md`, `docs/gaps.md`, `docs/saas/README.md` and
   `docs/saas/tracker.md`.
4. Read local `plan/STATUS.md` and `plan/execution-plan.md` when present.
5. Continue the first dependency-ready unchecked SaaS tracker task unless the user gives a more
   specific task.

## Product and architecture direction

- Preserve FigCraft's Rust document engine, renderer, command model, native app and Rust/WASM
  editor. This is an additive SaaS fork, not an editor rewrite.
- The fork-owned React/TypeScript code is limited to `webapp/`: browser routing, login, workspace
  and administration UI, HTTP integration and a typed lifecycle bridge around Rust/WASM.
- TypeScript does not own document semantics, commands, undo, conflict resolution or a second
  realtime protocol. Snapshots and collaboration batches cross the bridge as opaque data.
- Build one modular Rust cloud binary with `all`, `api`, `relay` and `worker` roles. Use PostgreSQL
  for tenant metadata and durable ordered edits, MinIO/S3 for immutable objects, and Redis only for
  replaceable presence, routing, leases, cache and rate limits.
- Acknowledged edits must be committed durably. One fenced authoritative sequencer owns a document
  at a time. A stale owner cannot write after losing its lease.
- AI uses a server-side provider-neutral relay and the existing command/collaboration path. Server
  WASM runs only in isolated workers with explicit capabilities and resource budgets.
- Support private single-tenant deployments, shared multi-tenant SaaS and embedding in another
  product's workspace from the same tenant-aware model.
- Teams are first-class groups inside a tenant: they have members, managers and invitations, and
  can own or receive access to workspaces, projects, documents and shared libraries. Authorization
  must revoke inherited access when team membership or a team grant changes.
- Keep fork additions in additive crates/directories and narrow adapters so upstream FigCraft can
  be merged regularly.

## Current repository state

- `main` was fast-forwarded from `origin/main` on 2026-10-16 before SaaS implementation resumed.
- The SaaS documentation baseline and 237-task tracker are committed under `docs/saas/`.
- S0 is at 14/15 tasks. `S0-012` is complete: `AGENTS.md` now permits narrowly scoped web tooling
  and defines its quality and licensing boundaries. The product owner explicitly accepted
  SAAS-ADR-001 through SAAS-ADR-016 as written for `S0-014`. `S0-015` verified
  `storytold/figcraft` from GitHub fork metadata, configured the local `upstream` remote and passed
  the first synchronization rehearsal with no incoming commits.
- S1 is in progress at 19/22 tasks. `S1-001` adds compileable workspace members for the modular
  cloud application and seven cloud libraries. `S1-002` defines distinct UUID-backed tenant,
  workspace, project, cloud-document, principal, object, job and request identifiers with strict
  parsing and UUIDv4 generation. `S1-003` adds infrastructure-neutral deployment, role, explicit
  permission, resource-state, quota and immutable tenant-context types. `S1-004` adds stable cloud
  error codes and a safe fixed-message public error envelope. `S1-005` pins domain wire formats and
  adds hostile-value and identifier property tests. `S1-006` extends dependency layering above the
  editor crates for cloud domain, adapters and application roles. `S1-007` adds strict CLI selection
  for `all`, `api`, `relay` and `worker` while making the missing runtime explicit. `S1-008` adds
  strict cloud environment parsing and unknown-key rejection. `S1-009` loads bounded mounted secrets
  through redacted wrappers and fixed diagnostics. `S1-010` rejects development URLs and inline
  credentials in production mode. `S1-011` adds bounded per-role health endpoints while keeping
  readiness false until the role runtime is initialized. `S1-012` adds generated request IDs,
  structured JSON request logs and validated W3C trace context propagation. `S1-013` builds a
  non-root distroless cloud image that runs with a read-only root filesystem. `S1-019` established the npm-pinned React,
  TypeScript and Vite workspace. `S1-020` defines the versioned opaque editor bridge with Rust/WASM
  as the sole owner of the browser collaboration WebSocket. `S1-021` adds the shared formatting,
  typecheck, lint, unit-test and production-build gate locally and in CI. `S1-022` enforces browser
  advisories, production dependency licenses and generated API-client drift. `S1-015` adds the safe
  deployment environment contract without claiming that the not-yet-built Compose stack runs.
  `S1-017` adds a dedicated cloud format, lint, unit-test and image-build CI job without replacing
  the full Rust, WASM and web gates.
- Commit `d2f244f` makes the pulled control-server cleanup test tolerate the expected macOS socket
  close race. Commit `f3856c5` records the scoped SaaS web policy.
- The full `cargo xtask ci` gate passed after the pull and test stabilization, including tests,
  layers, asset checks, WASM checks and plugin builds.
- The local `upstream` remote is `https://github.com/storytold/figcraft.git`, verified from GitHub's
  parent and source metadata. The 2026-10-16 rehearsal found upstream at `eb741a3`, already the
  fork's merge base, with no incoming commits.
- Architecture, security and operations reviewer identities remain intentionally unassigned in
  `S0-013`; never invent people or approvals.

## Continuation order

1. Assign real architecture, security and operations reviewers (`S0-013`) only when their
   identities are provided; never invent approvals.
2. Continue the requested `deploy/saas/` Docker and Compose foundation. `S1-014` needs the D-001
   dependency image release policy and release-matched MinIO initialization; do not claim a healthy
   SaaS application before its API/relay/worker runtime exists or guess those image versions.
3. Continue S1 in dependency order; update task evidence and milestone counts in the same commit as
   each completed task.

## Local environment notes

- Node 22.18.0 and npm 10.9.3 are available. Use npm for `webapp/` and pin the package manager in
  `webapp/package.json`; the local Corepack pnpm cache is incomplete.
- `../../craftrules/AGENTS.md` and the optional sibling `craft-fonts` checkout were absent during
  the last session. The repository still builds without `craft-fonts`, as required.
- Networked dependency installation may require sandbox approval. Never work around that by
  vendoring unknown packages or committing generated dependency directories.

## Before ending a work session

- Run the gates required by `AGENTS.md`. For web changes, run `npm ci --prefix webapp` and
  `npm run check --prefix webapp` in addition to `cargo xtask ci`.
- Update the SaaS tracker, this handoff if its state changed, and any affected roadmap or devlog.
- Keep one task or coherent arc per commit, include its milestone ID, and push `main`.
- Leave the tree clean or describe every intentional uncommitted file in `plan/STATUS.md`.

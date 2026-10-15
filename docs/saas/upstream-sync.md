# Upstream synchronization

> **Status:** required fork policy · **Last updated:** 2026-10-16

## Objective

This repository is a fork of `storytold/figcraft`. The SaaS platform must continue receiving
editor, format, collaboration, security and platform improvements from upstream without repeatedly
rewriting the cloud extension.

The current `origin` is the fork. Configure the original repository as `upstream`:

```sh
git remote add upstream https://github.com/storytold/figcraft.git
git fetch upstream --tags
```

If `upstream` already exists, verify it instead of adding another remote:

```sh
git remote -v
git remote get-url upstream
```

## Branch policy

| Branch | Purpose | Rewrite policy |
|---|---|---|
| `main` | Deployable fork, including accepted upstream and SaaS work | Never rebase after sharing |
| `saas/*` | Short-lived SaaS feature branches | Rebase or merge from current `main` before review |
| `upstream-sync/YYYY-MM-DD` | One upstream integration attempt | Delete after its merge; preserve the merge commit on `main` |
| `release/saas-X.Y` | Stabilization for a fork release when needed | Fixes only; merge fixes back to `main` |

Use a merge commit for upstream integration. It records the exact upstream boundary and makes the
next synchronization easier to audit. Feature branches may be rebased before review, but deployed
or shared branches are not rewritten.

## Conflict-minimizing layout

Fork-owned implementation belongs primarily in:

```text
apps/figcraft-cloud/
crates/cloud-*/
webapp/
deploy/saas/
docs/saas/
.github/workflows/saas-*.yml
```

Avoid adding SaaS state or dependencies to low-level editor crates. When cloud code needs existing
behavior, add a narrow upstream-compatible trait or public function and keep its implementation in
cloud crates.

Files with high upstream conflict risk include:

- Root `Cargo.toml` and `Cargo.lock`.
- `crates/engine/src/lib.rs` and command modules.
- `crates/collab-net/src/hub.rs`.
- `crates/ui-egui/src/lib.rs`.
- Root `ROADMAP.md` and upstream parity documents.
- Release workflows.

Cloud progress is tracked in this directory rather than adding fork milestones to upstream
roadmaps. Changes to high-risk files require an entry in the patch ledger below.

## Patch ledger

Record every intentional fork change to an upstream-owned area. Entries are added in the same
commit as the change.

| Patch ID | Upstream area | Purpose | Isolation or upstreaming plan | Status |
|---|---|---|---|---|
| FP-000 | None | Documentation baseline under `docs/saas/` | Additive fork-only directory | active |

Statuses are `active`, `submitted-upstream`, `accepted-upstream`, `reworked` or `removed`.

Good upstream candidates are generic improvements that benefit filesystem self-hosting and do not
mention SaaS products: asynchronous-neutral persistence interfaces, protocol negotiation,
large-asset channels, durability tests and clean public APIs. Tenant, billing, provider and hosted
control-plane behavior remains fork-owned.

## Synchronization cadence

- Fetch upstream at least weekly during active development.
- Integrate security fixes promptly.
- Integrate before starting a feature that touches a high-conflict file.
- Integrate before every fork release candidate.
- Do not combine an upstream merge with unrelated SaaS feature work.

## Rehearsal history

### 2026-10-16 — initial remote configuration

- GitHub's repository metadata identified `storytold/figcraft` as both the parent and source of the
  `schnsrw/figcraft` fork. The local `upstream` remote was configured as
  `https://github.com/storytold/figcraft.git` and fetched with tags.
- The rehearsal branch `upstream-sync/2026-10-16` started from fork commit
  `5bbce05ba9e6e784485d8637f23a12dd630731ac`. Upstream `main` was
  `eb741a362d2da36ab466991825b2c09c846496c8`, which was also the merge base.
- The fork was 11 commits ahead and upstream was 0 commits ahead. Incoming log, stat and file lists
  were empty. `git merge --no-ff upstream/main` therefore reported `Already up to date`; no empty
  merge commit was created and no conflict resolution was needed.
- With no incoming files, there was no collaboration protocol, stored-format, migration,
  dependency, licensing or asset-attribution impact to carry forward.
- `cargo xtask ci`, `npm ci --prefix webapp`, `npm run check --prefix webapp` and
  `npm run advisories:check --prefix webapp` passed on the temporary branch. The branch and worktree
  were deleted after verification.

## Synchronization procedure

### 1. Prepare

Ensure the fork main branch is clean and CI-green:

```sh
git switch main
git status --short
git fetch origin
git fetch upstream --tags
```

Create an integration branch from the fork:

```sh
git switch -c upstream-sync/2026-10-16 origin/main
```

### 2. Review incoming history

```sh
git log --oneline --decorate HEAD..upstream/main
git diff --stat HEAD...upstream/main
git diff --name-only HEAD...upstream/main
```

Identify changes to protocol, file format, server persistence, dependency versions, workspace
membership, CI, licensing and asset attribution before merging.

### 3. Merge

```sh
git merge --no-ff upstream/main
```

Resolve conflicts by preserving upstream behavior first, then reapplying the smallest cloud adapter
needed. Do not resolve generated or lock files without checking the source manifests that produced
them.

### 4. Verify

Run upstream gates:

```sh
cargo xtask ci
```

Run SaaS gates once implemented:

```sh
cargo test -p figcraft-cloud
cargo test -p figcraft-cloud-storage
cargo test -p figcraft-cloud-relay
docker compose -f deploy/saas/compose.yaml config
docker compose -f deploy/saas/compose.yaml up --wait
cargo test -p figcraft-cloud-e2e
```

The exact package names may consolidate, but the equivalent storage, relay and deployment suites
remain mandatory.

Review the web build and protocol compatibility whenever upstream changes `figcraft-collab`,
`figcraft-collab-net`, `figcraft-format`, `figcraft-engine` or `figcraft-web`.

The fork-owned `webapp/` has its own pinned package-manager lockfile, typecheck, lint, unit and
browser tests. An upstream merge does not regenerate that lockfile unless the Rust/WASM bridge or
generated API contract changed. Third-party package licenses remain part of the fork's release
review.

### 5. Record and merge

The integration pull request records:

- Previous upstream commit.
- New upstream commit.
- Conflict files and resolution decisions.
- Patch-ledger changes.
- Protocol or migration impact.
- Verification commands and results.

Merge the integration branch into `main` without squashing the upstream merge commit. Delete only
the temporary integration branch afterward.

## Protocol compatibility review

For every upstream collaboration change:

1. Identify its protocol range and capability changes.
2. Verify SaaS admission does not change message meaning.
3. Test previous supported client against the new relay.
4. Test the new client against the previous supported SaaS relay where compatibility is claimed.
5. Verify snapshot restore and event replay.
6. Update the minimum client version only through a documented release decision.
7. Roll relay changes out before clients require new optional fields.

## Database and object compatibility

Upstream `.figcraft`, collaboration snapshot or library encoding changes may affect stored SaaS
objects. The sync review must determine whether:

- Existing snapshots remain readable.
- New snapshots remain readable by the rollback release.
- A background rewrite is required.
- Old event payloads still replay.
- Export and library workers need a coordinated upgrade.

Never rewrite every stored object during application startup. Use a resumable migration or
on-access conversion that retains the previous verified object until completion.

## Dependency policy

- Prefer workspace dependencies for versions shared with upstream.
- Keep cloud-only dependencies in cloud crate manifests.
- Expect `Cargo.lock` conflicts and regenerate it from resolved manifests.
- Do not downgrade an upstream security fix to preserve a cloud dependency.
- Keep database drivers, HTTP frameworks and object clients out of existing low-level crates.
- Run license and advisory checks after upstream merges.

## CI separation

Upstream-compatible checks and SaaS checks are separate jobs but both gate `main`.

| Check group | Scope |
|---|---|
| Upstream | Existing `cargo xtask ci`, platform builds, layers, assets and WASM |
| SaaS unit | Cloud domain, auth, storage, relay, jobs, AI and WASM crates |
| SaaS integration | PostgreSQL, Redis and MinIO behavior |
| SaaS Compose | Image build, migration, health, collaboration and restart smoke tests |
| Compatibility | Supported desktop/browser clients and snapshot/event fixtures |
| Security | Tenant isolation, secret scanning, dependencies and container images |

An upstream failure is not waived because SaaS checks pass. A SaaS failure is not waived because
upstream remains green.

## Release versioning

- Retain the upstream source version in Rust packages unless compatibility requires a coordinated
  workspace version change.
- Tag fork releases in a distinct namespace such as `saas-v0.1.0`.
- Record the included upstream commit in release metadata and the About/support response.
- Container labels include fork release, source commit and upstream commit.
- Database schema and collaboration protocol versions are reported independently.

## Emergency upstream security update

1. Fetch and identify the smallest secure upstream boundary.
2. Create a dedicated upstream-sync branch.
3. Merge without unrelated feature work.
4. Run security-relevant upstream and SaaS tests first.
5. Build and scan release images.
6. Deploy to staging and exercise document open, edit, reconnect, export and AI-disabled paths.
7. Release and monitor.
8. Complete the full test matrix immediately after containment if the emergency path used a reduced
   pre-release suite.

## Sync acceptance gate

- The upstream merge commit is identifiable.
- No fork feature was silently dropped during conflict resolution.
- Patch ledger matches changes to upstream-owned files.
- Existing local and filesystem-server modes still work.
- SaaS migrations and stored-format compatibility are assessed.
- Upstream and SaaS CI pass.
- Release notes name any client, protocol or migration requirement.

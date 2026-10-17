<p align="center">
  <img src="assets/app-icon/figcraft-128.png" alt="" width="96" height="96">
</p>

<h1 align="center">FigCraft Cloud</h1>

<p align="center">
  <b>Self-hostable, browser-based collaborative interface design, built in pure Rust.</b><br>
  The FigCraft design editor in the browser, real-time co-editing on a durable cloud backend,<br>
  and the tenancy, sign-in and permissions to run it for one team or many.
</p>

<p align="center">
  <img alt="100% Rust" src="https://img.shields.io/badge/100%25-Rust-b7410e?style=flat-square&logo=rust">
  <img alt="Self-hosted with Docker Compose" src="https://img.shields.io/badge/self--hosted-Docker%20Compose-2496ED?style=flat-square">
  <img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20%2F%20Apache--2.0-3a3a3a?style=flat-square">
  <img alt="Status: early alpha" src="https://img.shields.io/badge/status-early%20alpha-d69e2e?style=flat-square">
</p>

<p align="center">
  <a href="https://schnsrw.github.io/figcraft/"><b>Website</b></a> ·
  <a href="docs/saas/README.md">Cloud documentation</a> ·
  <a href="docs/saas/tracker.md">Progress tracker</a> ·
  <a href="docs/UPSTREAM-README.md">Editor features</a>
</p>

<br>

<p align="center">
  <img src="docs/images/shot-7-collab.png" alt="A live FigCraft session on the Ridgeline app: three collaborators' avatars at the top, a "Priya Nair is spotlighting" banner with a Follow button, Priya's named cursor on a trail card, and a teammate's comment asking whether the Start hike button should stay pinned while scrolling" width="100%">
</p>

FigCraft Cloud is a fork of [FigCraft](https://github.com/storytold/figcraft), the open-source,
clean-room Rust reimplementation of the Figma design workflow. It keeps FigCraft's editor, renderer
and command model unchanged and adds a cloud platform around them. The same editor runs as a native
app and in the browser (WebAssembly); the cloud makes a document something a whole team opens from a
link and edits together, with every confirmed edit stored durably.

> **Status: early alpha.** Browser co-editing on the cloud stack works end to end, and identity,
> tenancy and authorization are complete. Multi-relay scaling, embedding, AI and production
> operations are in progress. The live, task-by-task picture is the
> [tracker](docs/saas/tracker.md).

## What it does

- **Co-edit in the browser.** Open a share link or sign in, and everyone in the document sees each
  other's cursors and edits live. Edits use FigCraft's own collaboration protocol: a server-ordered
  log with per-property last-writer-wins and per-person undo.
- **Never lose a confirmed edit.** The relay commits each batch to PostgreSQL before it confirms or
  broadcasts it, and a fencing token stops any stale relay from writing. Documents restore from a
  verified snapshot in S3-compatible storage plus the events after it.
- **Self-host it.** One Docker Compose stack: the cloud service, PostgreSQL, Redis, SeaweedFS (or any
  S3-compatible store) and a Caddy gateway. Branding (name, colours, logo, links) is set from
  environment variables, without rebuilding.
- **One team or many.** Single-tenant installs and multi-tenant SaaS share one schema, with
  PostgreSQL row-level security on every tenant table.
- **Sign in your way.** OIDC login with PKCE and back-channel logout, SAML through an identity broker
  such as Keycloak, browser sessions, service accounts with scoped credentials, and expiring share
  links.
- **Permissions that hold.** Tenant, team, workspace and document roles with inherited team grants,
  deny-by-default checks, explicit sensitive permissions, audited support access, and revocation
  that ends live sessions and relay connections.
- **Bring existing work.** A resumable importer moves documents, libraries and plugins from a
  self-hosted `figcraft-server` and verifies them before cutover.

## Quick start

You need Docker with Compose, and about 8 GB of free RAM and 20 GB of disk for the first image
build.

```sh
git clone https://github.com/schnsrw/figcraft.git
cd figcraft
# Create the secret files the stack expects (see deploy/saas/README.md), then:
docker compose --env-file deploy/saas/env.example -f deploy/saas/compose.yaml up --build -d
# Create a document and print a link that opens it in the browser editor:
docker compose -f deploy/saas/compose.yaml exec cloud figcraft-cloud --create-share-link
```

Open the printed `http://127.0.0.1:8080/edit/#join=…` link in two browser tabs to co-edit.
[`docs/saas/core-slice.md`](docs/saas/core-slice.md) walks through it, and
[`docs/saas/deployment-operations.md`](docs/saas/deployment-operations.md) covers production
settings, backups and upgrades.

The desktop app, CLI and the self-hosted `figcraft-server` from upstream FigCraft are all still
here and still work. Build and run instructions are in the
[upstream README](docs/UPSTREAM-README.md#get-started).

## Documentation

| Topic | Where |
|---|---|
| Overview and decisions | [`docs/saas/README.md`](docs/saas/README.md) |
| Architecture and consistency model | [`docs/saas/architecture.md`](docs/saas/architecture.md) |
| Data, tenancy and integration contracts | [`docs/saas/data-tenancy-integration.md`](docs/saas/data-tenancy-integration.md) |
| Security model | [`docs/saas/security.md`](docs/saas/security.md) |
| Deployment and operations | [`docs/saas/deployment-operations.md`](docs/saas/deployment-operations.md) |
| Branding and configuration | [`docs/saas/branding-and-configuration.md`](docs/saas/branding-and-configuration.md) |
| Relay performance and sizing | [`docs/saas/relay-performance.md`](docs/saas/relay-performance.md) |
| Roadmap and tracker | [`docs/saas/roadmap.md`](docs/saas/roadmap.md), [`docs/saas/tracker.md`](docs/saas/tracker.md) |
| The FigCraft editor itself | [`docs/UPSTREAM-README.md`](docs/UPSTREAM-README.md), [`ROADMAP.md`](ROADMAP.md) |

## How it is built

Everything is Rust except a small React/TypeScript account shell in `webapp/`. The cloud adds
crates beside the editor's, so upstream FigCraft merges in cleanly:

- `apps/figcraft-cloud`: one binary with `api`, `relay` and `worker` roles (axum API, the
  collaboration relay, CLI commands for migrations, bootstrap and share links).
- `crates/cloud-storage`: PostgreSQL repositories, migrations and S3 object storage.
- `crates/cloud-relay`: the durable collaboration backend for FigCraft's collaboration hub.
- `crates/cloud-auth`: OIDC, session policy and the authorization rules.
- `crates/cloud-domain`: identifiers, tenancy types and the public error envelope.
- `deploy/saas`: the Docker Compose stack, gateway and image builds.

## Contributing

Issues and pull requests are welcome. Work is planned in [`docs/saas/tracker.md`](docs/saas/tracker.md)
and the agent handoff in [`CODEX.md`](CODEX.md). The editor follows upstream's rules in
[`CLAUDE.md`](CLAUDE.md) and [`AGENTS.md`](AGENTS.md): clean-room behaviour only, no third-party
assets, no panics in shipped code, everything is a command. Upstream changes are merged regularly
([`docs/saas/upstream-sync.md`](docs/saas/upstream-sync.md)).

Coding agents can navigate the code through [`tools/codegraph`](tools/codegraph/README.md), an
incremental code graph with a CLI and a read-only MCP server (registered in `.mcp.json`).

## License and credits

Code and original assets are dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE),
at your option. Required notices are in [NOTICE](NOTICE).

FigCraft Cloud is based on FigCraft by the ArtCraft team and the FigCraft contributors. It is an
independent fork and is not made, sponsored or endorsed by the ArtCraft team.

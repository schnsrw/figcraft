# FigCraft SaaS web application

This npm workspace contains the fork-owned browser shell around the existing Rust/WASM editor.
Its architecture boundary is defined in the root `AGENTS.md` and `docs/saas/README.md`.

The browser shell owns routing, authentication, workspace and administration experiences. It does
not implement document semantics, editing commands, undo, conflict resolution or collaboration.
Those remain in Rust, and the editor bridge treats snapshots and operation batches as opaque data.

## Commands

Run from the repository root:

```sh
npm ci --prefix webapp
npm run check --prefix webapp
npm run dev --prefix webapp
```

`check` runs formatting verification, strict type checking, ESLint, unit tests and the production
build. It also checks production dependency licenses and verifies that a generated API client is
current once `packages/api-client` exists. Use `npm run format --prefix webapp` to apply the
repository's web formatting rules.

CI additionally runs `npm run advisories:check --prefix webapp`, which fails for high or critical
npm advisories. The license gate reads the committed lockfile, refuses missing or unknown licenses
for production dependencies and rejects GPL, AGPL and LGPL even when an alternative license is
listed.

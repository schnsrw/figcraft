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

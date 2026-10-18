# FigCraft active session handoff

> Updated 2026-10-18. Start here, then read `AGENTS.md`, `CODEX.md`, `ROADMAP.md`,
> `docs/gaps.md`, `docs/saas/tracker.md` and
> [`docs/saas/wasm-first-browser-decision.md`](docs/saas/wasm-first-browser-decision.md).
> This file records the active combined-branch work. Older continuation instructions in
> `CODEX.md`, `plan/STATUS.md` and `plan/execution-plan.md` predate the branch and the browser
> architecture correction.

## Owner's current decision and scope

The owner stopped the prior UI implementation and then corrected the browser architecture:
**build the browser product on FigCraft's existing Rust/WASM application**. Connect it to the
cloud for authenticated HTTP, document/workspace metadata and collaboration through narrow
adapters. Use its home, templates, document tabs and editor as one experience. The owner
explicitly rejected the React-owned file browser, iframe, CSS-only redesign and the impression
that the editor is a page inside a second product. The owner requested **documentation and code
analysis only**, then asked to **push those docs and check the backend's condition**. No
application implementation was requested in this continuation.

The earlier combined-branch audit remains unfinished: S6 and priority TS work, team/project
visibility, version caps, large images and the editor path all need review. The owner previously
asked for visible-browser evidence, a complete UX/layout/workflow result rather than CSS changes,
sign-in without ordinary-desktop scrolling, `rem` CSS units, and usable local credentials. Those
are historical implementation constraints if work resumes; the new WASM-first direction controls
the browser architecture.

## Why we changed direction

We assigned account/home/workspace/navigation and the editor's lifecycle to a React shell before
mapping existing Rust UI capabilities. React then duplicated file navigation, mounted the WASM
editor in an iframe and hid Rust tabs and Share to avoid duplicate chrome. This split the product
into two navigation and visual systems. The owner rejected the result. A local iframe screenshot
also shows editor text/tools at roughly half the standalone size; the cause is unknown, so do not
claim a DPI diagnosis. The [decision record](docs/saas/wasm-first-browser-decision.md) contains
an evidence table, architecture boundaries, migration phases, risks and acceptance criteria.

## Code findings that matter for implementation

- `crates/ui-egui/src/home.rs`: Recents, Online, Templates and Rust-rendered starter template
  thumbnails. Template selection currently creates a local document through
  `file.newFromTemplate`.
- `crates/ui-egui/src/tabs.rs`: Home plus a tab per open file. `Session::empty()` supports a home
  startup with no unrelated Untitled tab. `crates/engine/src/live.rs` opens joined documents in
  tabs. `crates/collab-net/src/web.rs` can keep multiple Rust-owned browser WebSocket sessions.
- `crates/ui-egui/src/online.rs`: old `collab.files`/`collab.openFile` listing of files remembered
  in preferences. **This is not the authenticated tenant cloud catalog.** Cloud home and file
  lists require new integration.
- `apps/figcraft-web/src/web.rs`: one `FigApp` in the WASM runner; Rust `NetLink`; `#join` handling;
  browser services and branding. The uncommitted `embedded_cloud` mode in this app and
  `crates/ui-egui/src/{lib,right}.rs` hides Rust tabs, home and Share in the React iframe. It
  conflicts with the target. `pending_name` is global/one-shot and insufficient for multi-file
  cloud tabs.
- Cloud API: `GET /api/v1/auth/session`, `/tenants`, `/workspaces`,
  `/workspaces/{id}/projects`, `/projects/{id}/documents` and
  `/documents/{id}/collab-link` exist. Browser sessions normally receive short-lived realtime
  tickets for collaboration; `POST /documents/{id}/realtime-ticket` also exists. Tickets last
  60 seconds. A code search found no ticket refresh in `apps/figcraft-web` or
  `crates/collab-net`; its driver retries from the existing invite, so reconnect after expiry is
  a concrete gap. Cloud auth is a server-managed HttpOnly cookie; write requests have same-origin
  checks. The API remains the authorization authority.
- Current deployment serves React at `/` and WASM at `/edit/` in
  `deploy/saas/{web.Caddyfile,web.Dockerfile}`. Stable workspace/document URLs and sign-in
  return paths need migration, not a blind route swap. Host embedding still needs origin/token
  protections and a narrow handshake.
- `crates/ui-egui::Services` currently covers file dialogs, clipboard, downloads and platform
  functions but has no cloud catalog/auth API. A new bounded browser/cloud seam is needed at
  the app/UI boundary without moving HTTP into the engine or creating a second document model.

## Repository and worktree

- Directory: `/Users/sachin/Desktop/melp/services/figcraft`.
- Branch: `combined-2026-10-18`; HEAD `4f6d06c`; `main` untouched. The worktree is dirty.
  Preserve all changes. Do not run the stale clean-tree `git pull` instruction.
- The earlier application changes have no commit or push from this combined-branch session.
  The owner said stop during the previous build/checks; those processes were interrupted. The
  documentation arc is being committed separately. Do not claim the application branch is
  verified from that earlier run.
- Uncommitted code groups: cloud API/config and storage visibility/version-cap changes; relay and
  engine formatting/large-image changes; browser Rust `#join`/`embedded_cloud` work; React
  account/home/workspace/file/editor and CSS redesign; web tests; locale key. The new
  `crates/cloud-storage/tests/version_cap.rs` is untracked. `SESSION_HANDOFF.md` itself is
  untracked. Use `git status --short` for the exact current list.
- Documentation changed in this documentation-only pass: `CODEX.md`, `docs/saas/README.md`,
  `architecture.md`, `product-requirements.md`, `roadmap.md`, `tracker.md`,
  `web-workspace.md`, `proposal-review.md`, and the new decision record. No application code
  was changed in this pass.
- `../../craftrules/AGENTS.md` is absent in this checkout. `plan/STATUS.md` and
  `plan/execution-plan.md` retain stale 2026-10-16 details; this pass added supersession notices
  to both local, gitignored files. The asset and clean-room restrictions in root `AGENTS.md`
  remain absolute; no incumbent or third-party product assets.

## Earlier code work and evidence (before this documentation pass)

- Team/project visibility: `crates/cloud-storage/src/resource_admin.rs` and
  `apps/figcraft-cloud/src/api/documents_api.rs` were changed so a project-only team member can
  discover the parent workspace while project rows stay permission-filtered. Four relevant
  database integration tests later passed against a disposable test database.
- Version cap: `crates/cloud-storage/src/versions.rs` locks the document row before counting and
  inserting versions; the new concurrent cap test passed against that disposable database.
- A fresh backend check on 2026-10-18 reran `team_projects` (4/4 passed) and `version_cap`
  (1/1 passed) with `FIGCRAFT_TEST_DATABASE_URL_FILE=/private/tmp/figcraft-audit-db-url`.
- The React sign-in and workspace styling/editing pass passed `npm ci --prefix webapp` and
  `npm run check --prefix webapp` (160 SaaS tests at the last successful run). This verifies the
  current React code, not the WASM-first target. No new application code was changed this pass.
- `cargo xtask wasm` passed during the previous implementation session. A focused i18n test
  passed after the missing export string was added. The full `cargo xtask ci` was interrupted
  when the owner said stop; a Docker web build was also interrupted during WASM optimization.
  On the documentation/push continuation, `cargo xtask ci` passed formatting and Clippy and
  reached the Rust test suite, but stopped at
  `i18n::tests::catalogs_have_known_keys_and_matching_placeholders`: the earlier uncommitted
  English `Couldn't download the export` key has no Spanish translation. This pass does not
  change application code. A separate clean-HEAD checkout also stopped at `cargo fmt --check`:
  the combined branch's committed base has unformatted files such as
  `crates/collab-net/src/http.rs`; the dirty tree already contains formatting-only fixes for
  those. Therefore neither the dirty tree nor clean HEAD has a passing full CI gate. The docs
  may be pushed as an explicitly unverified branch update; the application tree still needs its
  localization and formatting changes reconciled and a full gate rerun.
- A visible-browser same-origin iframe probe at local port 8080 loaded the Rust editor and
  showed account access and close behavior. The frame screenshot
  `/private/tmp/figcraft-embedded-editor.png` shows the small rendered UI. Standalone comparison:
  `/private/tmp/figcraft-editor-current.png`. Other browser captures include
  `/private/tmp/figcraft-current-{home,workspace,file}.png`. These are local diagnostic images,
  not distributable assets. Earlier app screenshots showed the UI shortcomings the owner
  identified. The new architecture has **not** been visually verified.
- The local test-stack owner login is `owner`; the password is only in
  `/private/tmp/figcraft-local-owner-credentials.txt` (mode 0600). Do not put it in tracked docs,
  logs or screenshots. On 2026-10-18, `docker ps` showed the `ts-run` gateway, cloud, web,
  PostgreSQL, Redis and SeaweedFS containers healthy. Through the gateway at
  `http://127.0.0.1:8080`, `/healthz`, `/api/v1/healthz` and `/readyz` each returned HTTP 200.
  `GET /api/v1/auth/methods` returned 200; signed-out requests to `/auth/session` and
  `/workspaces` returned the expected 401. This confirms the public gateway and basic auth
  boundary, not an authenticated create/edit flow.
  An initial sandboxed curl failed because sandbox localhost access was blocked; a permitted
  direct probe established the backend was running. Health at this instant is not a complete
  end-to-end auth/edit test. The disposable test database URL is in a private temporary file;
  do not print it.

## Implementation plan for a later session

1. Read the [decision record](docs/saas/wasm-first-browser-decision.md), inspect the dirty diff and
   preserve the existing work. Reconcile the old React/iframe changes only after the Rust/WASM
   replacement is viable.
2. Restore the Rust editor's tabs/home/Share for cloud sessions. Establish a cloud-root startup
   using `Session::empty()`, while preserving direct-link joins and standalone behavior.
3. Add a bounded same-origin HTTP cloud adapter in the browser app and cloud metadata UI at the
   egui layer. Handle session states, tenant choice, paging, permissions, error/retry states,
   workspaces, projects and documents. Keep Rust `NetLink` as the sole realtime owner.
4. Open cloud documents as Rust tabs in one app, with stable cloud ids, per-file names/roles,
   already-open focusing, tab close behavior and fresh authorized invites. Implement ticket
   renewal before expired-ticket reconnect. Prove two concurrent documents, reconnect after 60
   seconds, revocation and deep-link reload in a real browser.
5. Make creation/templates and sharing cloud-aware; then move account/admin flows into the
   WASM-first experience. Keep temporary React paths only where needed during migration.
6. Move the primary route to WASM without breaking sign-in return, workspace/document URLs,
   embedding and static asset loading. Verify full journeys in a visible browser, including
   desktop/tablet layout and accessibility. Update tracker evidence only after the new path works.
7. Resume the S6/TS audit, including large-image fallback against the relay event limit.
   Run the repository's applicable quality gates, then commit coherent arcs with milestone ids.

## Do not overclaim

- The new Rust/WASM cloud home, account UI and route migration do not exist yet.
- The current Rust Online section is not a cloud workspace browser.
- Existing React checks and screenshots are not acceptance evidence for SAAS-ADR-018.
- The dirty combined branch has not passed its final full CI or real-browser migration run.
- No tracker checkbox or headline milestone count changed for this documentation decision.

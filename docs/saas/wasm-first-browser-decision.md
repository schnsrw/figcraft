# Browser product direction: one FigCraft Rust/WASM application

> Decision by the product owner, 2026-10-18. This is the target architecture, **not a claim that
> it has been implemented**. It supersedes the browser composition in SAAS-ADR-014. The current
> deployment still serves a React application at `/` and the FigCraft WASM application at `/edit/`.

## Revision, 2026-10-18 evening: the editor is WASM, the file browser may stay HTML

The owner refined the decision. The goal is a self-hosted clone of the hosted design tool's
workflow, and what must not break is the **editing experience and the impression of one product**.
Some screens may stay in React. The split now follows the incumbent's public, observable product:

- **File browser (HTML; React today):** sign-in, setup, invitations, home and recents, workspaces,
  teams, projects, file lists, templates, sharing from a file's menu, people, account and settings.
- **Editor (Rust/WASM, full page, no iframe, no host chrome):** one cloud file at its own URL,
  `/workspace/{workspace}/file/{document}`. The web server serves the WASM editor at that path;
  the editor reads the ids from the path, asks the cloud API for the file's name, project, share
  permission and collaboration invite, and joins the live session. In this *cloud editor mode*
  (`crates/ui-egui/src/cloud.rs`) there is no Home and no tab strip (the browser tab is the file's
  tab); the main menu starts with **Back to files**; the left panel shows the project under the
  file name; Share copies the file's link; local file commands (Open, Save, Save as, Open recent,
  Close tab, Join/Publish live session, local versions and branches) are hidden from menus, the
  Actions palette and shortcuts. Save says changes are saved automatically.
- **Handoff:** the file browser navigates to a file with a full page load; React's own file page
  and iframe were removed. Recents are recorded by the file browser when a file is opened.
- **One look:** the file browser takes its colours, type and radii from the editor's theme tokens
  (planned next: generated from `theme::Tokens`), so the two read as one product.

The sections below remain the analysis that led here. Where they assign home, templates and
navigation to the Rust UI, this revision replaces them: those stay in the file browser, with the
API work listed in "Remaining work" below.

### Remaining work (in order)

1. Shared design tokens between the editor and the file browser; restyle the file browser.
2. Realtime ticket renewal before a reconnect (tickets last 60 seconds).
3. An editor-context endpoint (file, breadcrumb, role, permissions and invite in one call).
4. Full Share dialog in the editor (members, team access, links) on the existing APIs.
5. Templates from the API (`GET /templates`, create a document from a template on the server),
   duplicate, move, per-user recents and drafts, file thumbnails.
6. Cloud version history in the editor (replacing the hidden local version commands).

## Decision and the mistake

The authenticated browser product should be the existing FigCraft Rust/egui application compiled
to WASM. Its home, file tabs, templates, canvas, panels and collaboration should form one continuous
experience. Add the essential browser and cloud communication to that application: authenticated
HTTP for accounts and cloud metadata, the existing Rust-owned WebSocket client for live documents,
and a narrow host interface where embedding genuinely requires one. Use the same Rust UI and
document behavior across browser and native clients where the platform permits.

We made an architectural mistake by assigning home, workspace navigation and the editor's outer
lifecycle to a separate React application before checking how much of that workflow FigCraft
already owned. The React file browser duplicated home/navigation, opened each document through an
iframe, and required a parent toolbar to compensate for hiding the editor's tabs and Share control.
This added a second set of navigation state, visual hierarchy and lifecycle rules. Our successive
CSS and layout passes improved individual React screens but could not produce the requested single
design-tool experience. The owner rejected that direction. The iframe's UI also appeared much
smaller than the standalone editor in a local browser capture; its cause has **not** been proven.
Do not describe this as a device-pixel-ratio diagnosis.

This decision does **not** roll back cloud authorization, storage, relay, tenant APIs or the Rust
editor. It changes which client owns the browser product UI. Existing React code remains in the
working tree until its useful account/admin flows are replaced or deliberately retained as a
temporary migration aid; its presence is not evidence that the target architecture is complete.

## Findings from the current code

| Area | Existing behavior and evidence | Gap or consequence |
|---|---|---|
| Home and templates | `crates/ui-egui/src/home.rs` has Recents, Online and Templates. Template previews are rendered from Rust documents; selecting one dispatches `file.newFromTemplate`. | A template creates a local document today. Cloud creation and persistence need an explicit flow. |
| Tabs | `crates/ui-egui/src/tabs.rs` shows Home and one tab per open document. `Session::empty()` can start on home without an unrelated Untitled file. | The uncommitted `embedded_cloud` mode in `crates/ui-egui/src/lib.rs` hides this strip for cloud joins. Reverse that behavior during implementation. |
| Live documents | `crates/engine/src/live.rs` opens a guest document in a new tab on welcome. `crates/collab-net/src/web.rs` says its `NetLink` can join multiple sessions in one browser app; Rust owns their sockets. | Confirm open/reopen/close and per-tab status in a real browser, especially a two-document case. Keep one connection owner per document. |
| Existing Online section | `crates/ui-egui/src/online.rs` lists files remembered in local preferences and published through `collab.files`/`collab.openFile`. | It is **not** an authenticated, tenant-scoped list of cloud workspaces, projects and documents. Reuse presentation patterns, not its storage semantics. |
| Browser startup | `apps/figcraft-web/src/web.rs` creates one `FigApp`, installs `NetLink`, reads a `#join` invite and clears the fragment. Its `WebServices` handles browser-only facilities. | Startup currently uses `Session::new()` for a root page; decide the cloud-home initialization and deep-link path. Add bounded same-origin cloud API calls at this app/adapter seam. |
| Cloud document open | `GET /api/v1/documents/{id}/collab-link` returns an invite and editor URL. Browser sessions normally receive a 60-second realtime ticket. The Rust network driver retries connections using its existing invite. | A reconnect after ticket expiry needs a fresh authorized ticket before retry; no implemented browser ticket-replacement path was found. Tie each invite to a stable document id and test expiry/revocation. Never persist secrets in history, logs or local storage. |
| Cloud metadata | `/auth/session`, `/tenants`, `/workspaces`, `/workspaces/{id}/projects` and `/projects/{id}/documents` are available. React has typed paged clients and explicit session states. | Port only needed cloud metadata and UI states into Rust; keep document snapshots and collaboration batches in the existing Rust model. Check tenant headers, paging limits and role-driven actions. |
| Deployment | `deploy/saas/web.Caddyfile` and `web.Dockerfile` serve React at `/` and WASM at `/edit/`. The React file screen currently mounts `/edit/` in an iframe. | The entry route and static build need a deliberate migration. Keep stable document links and embedding contracts; serving WASM at `/` alone does not provide those. |
| Current browser limitation | `apps/figcraft-web/index.html` is a single full-window canvas. `crates/ui-egui/src/lib.rs` has no cloud catalog service in `Services`. | Account, workspace, project, document, permissions, search and admin UI are not already wired into the WASM product. |

## Intended boundaries

1. **Rust engine and UI:** the document model, commands, undo, canvas, home, templates, tabs,
   file navigation and product interaction state. Cloud-specific UI belongs in the swappable
   `ui-egui` layer, with commands for user-visible document operations where appropriate.
2. **Browser adapter (`apps/figcraft-web`):** same-origin authenticated HTTP using the browser's
   cookie, URL/history integration, browser storage where truly needed, clipboard/downloads,
   and handoff to `FigApp`. Keep the existing Rust `NetLink` as the only owner of collaboration
   WebSockets. Bound response sizes, paging and displayed strings.
3. **Cloud service:** account and tenant authority, permissions, metadata, collaboration tickets,
   durable edits, object storage and jobs. The client may show a server-provided `can` summary but
   the service remains authoritative on every action.
4. **Host integration:** if embedding needs JavaScript, keep it a small versioned lifecycle and
   identity handshake around the WASM app. Do not create another workspace UI or collaboration
   client. The host product's own chrome is outside FigCraft's editor canvas.
5. **Native:** share the Rust UI where possible, but keep browser cookies and browser APIs in the
   browser adapter. A native cloud transport/auth adapter is separate work.

## Migration plan, in dependency order

These are planned changes, not completed tracker items.

1. **Freeze and preserve evidence.** Keep the dirty combined branch intact, record the previous
   browser screenshots and test results, and distinguish current React behavior from target WASM
   behavior. Do not delete the old shell before replacement works.
2. **Restore a complete editor view.** Remove cloud-only hiding of the Rust tab bar and Share
   control. Start the hosted WASM entry at the Rust home screen; a direct document link opens its
   file in a Rust tab. Keep presentation/full-screen modes explicit and test canvas size at normal
   browser zoom.
3. **Add a narrow cloud catalog adapter.** Authenticate via the existing cookie/session endpoint,
   resolve tenant choice, list workspaces/projects/documents through bounded paged HTTP, and feed
   those metadata into Rust home/navigation. Add loading, empty, offline, forbidden and retry
   states. Do not attach cloud metadata to `Prefs::online_files` or duplicate document state.
4. **Open cloud files in existing tabs.** Fetch an authorized collaboration invite for the chosen
   document, call the existing join path in the same `FigApp`, associate document id/name/role with
   the opened tab, handle already-open files, and prove two tabs can stay live. Address the current
   one-shot global `pending_name` in `apps/figcraft-web/src/web.rs`; cloud names must be tracked
   per file or sourced from durable metadata. Keep signed links out of address-bar history after
   use. Implement ticket renewal before a reconnect can reuse an expired 60-second ticket;
   `webapp/packages/editor-bridge` declares `replaceSessionTicket` but that contract alone does
   not implement renewal in the current Rust browser client.
5. **Make creation and templates cloud-aware.** Decide when New or a starter template remains a
   local draft and when it creates a cloud document. Preserve command semantics and document
   identity; do not silently create an unrelated local copy of a cloud file. Support open, rename,
   archive, restore and share through authorized APIs and command/UI seams.
6. **Move account and administration journeys.** Cover sign-in, bootstrap, tenant selection,
   people, teams, invitations, account, settings and required errors in the Rust/WASM experience.
   Keep the existing React screens temporarily only where the replacement is not yet ready;
   remove duplicate routes after real-browser acceptance. Respect the existing same-origin POST
   checks and HttpOnly session cookie.
7. **Switch the entry route.** Build and serve WASM as the primary browser app, preserve stable
   workspace/document URLs and sign-in return paths, and update the deployment, redirects,
   cache/CSP and embedding contract. Test direct reload, back/forward, multi-tab navigation,
   auth loss, link expiry and two simultaneous editors before retiring the React shell.
8. **Verify and record.** Run WASM, Rust and any remaining web gates; test against the local cloud
   stack in a visible browser at desktop and tablet widths; include keyboard/screen-reader review.
   Only then update tracker checkboxes, roadmap status and release claims. Commit coherent arcs
   after their required gates.

## Risks and open design questions

- **Accessibility:** a canvas UI does not automatically satisfy the existing WCAG and screen-reader
  target. The Rust UI needs browser accessibility evidence before removing accessible HTML account
  flows. Treat this as a release gate, not a reason to keep duplicate navigation indefinitely.
- **Auth state:** `GET /auth/session` can mean signed out, bootstrap required, password change,
  tenant choice, unsupported credential or service unavailable. The WASM shell must distinguish
  them. A cookie is never exposed to Rust or JavaScript; same-origin `fetch` carries it.
- **Multi-tab identity:** a cloud document id and UI tab id are different. Reopening a document
  should focus its existing tab; closing one must end only its own connection. Refresh/deep link
  should restore the intended cloud document without replaying an expired secret.
- **Ticket lifecycle:** the server's `collab-link` route comment says the editor fetches a fresh
  ticket to reconnect, but a code search found no `/realtime-ticket` call or ticket replacement in
  `apps/figcraft-web` or `crates/collab-net`. The driver reconnects from its existing invite.
  Treat renewal as a missing implementation and verify it with a connection dropped after expiry.
- **Cloud names and permissions:** the current placeholder rename happens once for a single
  `#join` file, and the old Share popover targets a different server flow. Both need cloud-aware
  behavior. Server permission checks and revocation must remain effective during a live session.
- **Templates and local-first behavior:** local templates are valuable; users need an explicit
  save/create-in-workspace path. Browser persistence/offline behavior must be specified before
  claiming a local draft is durable.
- **Embedding:** the external host still needs origin-restricted token exchange and a bounded
  handshake. One WASM UI does not remove the security requirements of an iframe embedding case.
- **Upstream merges:** keep cloud HTTP/auth adapters fork-owned and limit changes to shared editor
  crates to small UI hooks. Follow the existing command and layering rules.

## Acceptance evidence for the pivot

The pivot is complete only when a person can open the primary site, sign in, browse a cloud
workspace, create a document from blank or template, open two cloud documents as FigCraft tabs,
edit and reconnect both, share according to their role, return to home, reload a direct file URL,
and sign out, all in one coherent Rust/WASM product. The cloud API must still reject forbidden
operations. The UI must be inspected at normal desktop and tablet sizes and pass the applicable
accessibility, Rust/WASM, web and real-browser gates. No such end-to-end evidence exists yet.

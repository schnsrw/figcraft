# Web workspace (`webapp/saas`)

The browser application a person signs in to: files, workspaces, people and sharing around the
Rust/WASM editor. It is a React/TypeScript single-page app (the repository's one TypeScript
exception) served by the web container at `/`, with the editor at `/edit/`. It holds no document
semantics; it calls the same `/api/v1` the other clients use and never keeps a credential (the
session cookie is `HttpOnly`).

## Screens and stable URLs

| URL | Screen |
|---|---|
| `/` | Recent files (this browser only) and workspaces |
| `/workspace/{workspace_id}` | Projects and files; `?project={project_id}` selects a project |
| `/workspace/{workspace_id}/file/{document_id}` | One file: your role, **Open in editor**, **Share** |
| `/people` | Members and roles, teams, invitations |
| `/invite#token=…` | Accept an invitation (the token stays in the URL fragment) |

Identifiers must be canonical UUIDs; anything else is the not-found page without calling the API.
Every URL survives a reload and can be shared. A visitor who is signed out gets a sign-in page that
sends them back to the same URL: the app sends the server only a same-origin absolute path as
`return_to` (the server validates it again), never a full URL, and an invitation token is parked in
session storage for the round trip and removed from the address bar on arrival.

## First run and sign-in

1. Sign-in (`GET /api/v1/auth/login?return_to=…`) needs an OIDC provider. Before navigating, the
   page asks the server to begin sign-in without following it: a redirect means the provider is
   configured and the browser then goes; a `503` is explained on the page ("the identity provider
   is not configured or cannot be reached") instead of showing a bare error document.
2. A brand-new single-tenant deployment has no administrator. After signing in, the person sees
   **Set up your workspace** and enters the one-time setup token the operator printed with
   `figcraft-cloud --print-bootstrap-token`; the app calls `POST /api/v1/auth/bootstrap/complete`
   and the session becomes the owner's. A wrong or used token gets a clear message and the form
   stays.
3. Multi-tenant deployments ask which organisation when a person belongs to several, remember the
   choice in local storage, and let someone with none create one or open an invitation link.

## Permissions in the UI

The server decides every action. The UI only avoids offering what would be refused:

- Workspace, project and file reads carry a `can` object (`GET /workspaces`, `/workspaces/{id}`,
  `/projects/{id}`, `/documents/{id}`) built from the same permission check the routes use, and the
  screens show rename, archive, restore, create and share controls from it.
- Tenant-level controls (creating workspaces, changing roles, inviting) follow the tenant role from
  `GET /auth/session`; owners are the only ones who can grant or change ownership.
- A refusal from the API is shown with its fixed message and request reference.
- A `409 resource_inactive` on a workspace route shows "This workspace isn't available" (suspended
  or archived).

## Trying it against an identity provider

The Compose stack has no identity provider of its own, and the app deliberately has no sign-in
bypass. To use the app end to end, run any OpenID Connect provider (for local trials:
Keycloak, Authentik, Dex, Zitadel or a hosted tenant) and point the deployment at it:

```sh
# deploy/saas/.env (or the orchestrator's environment)
FIGCRAFT_PUBLIC_URL=https://figcraft.example        # the address the browser uses
OIDC_ISSUER=https://identity.example.com/realms/figcraft
OIDC_CLIENT_ID=figcraft
OIDC_REDIRECT_URL=https://figcraft.example/api/v1/auth/callback
# secret file: OIDC_CLIENT_SECRET_FILE=/run/secrets/oidc_client_secret
SESSION_SIGNING_KEY_FILE=/run/secrets/session_signing_key   # at least 16 bytes; also keys webhook secrets
```

Register `OIDC_REDIRECT_URL` as an allowed redirect URI at the provider, use the authorization-code
flow with PKCE, and make sure ID tokens carry `email` and `email_verified` if people will accept
invitations (an invitation is matched to a verified email). Then, for a fresh single-tenant
database: start the stack, run `figcraft-cloud --print-bootstrap-token`, open the site, sign in and
enter the token.

## What has not been exercised end to end

- No provider was available while this was built, so the browser flow from provider redirect back
  through `/api/v1/auth/callback` into the app was not run against a real IdP. What is covered:
  the API client, session states, routing, the sign-in probe and every screen against a fake API
  that returns the documented response shapes (`webapp/saas/src/**/*.test.tsx`), and the server's
  own route tests for the endpoints used.
- Not built in this block: libraries, search, notifications and settings (the rest of S6-020), the
  editor mount and bridge (S6-022 to S6-024), and the WCAG 2.2 AA audit with assistive technology
  (S6-025). Keyboard and ARIA behaviour is tested (tabs, dialogs, focus return, live regions, skip
  link) but no screen-reader or automated contrast audit has been run.
- Cross-site embedding cookies and CSP per host belong to S6-009.

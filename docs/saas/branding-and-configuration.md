# Branding and configuration

FigCraft Cloud is configured entirely through container environment variables and mounted files.
This page covers runtime branding (CS-005) in full and points at the existing cloud settings.

## Runtime branding

A deployer changes the product name, colours, logos and links **without a code change or an image
rebuild**: set `FIGCRAFT_BRAND_*` on the `web` service and restart that container
(`docker compose up -d web`).

How it works:

1. The web image ships `deploy/saas/branding.json.tmpl` as `/etc/caddy/branding/branding.json`.
   Caddy's `templates` handler renders it on every request to `/branding.json` from the web
   container's environment (`deploy/saas/web.Caddyfile`), with `Content-Type: application/json`,
   `Cache-Control: no-cache` and `X-Content-Type-Options: nosniff`, and no `ETag`/`Last-Modified`
   (a restart is visible on the next page load).
2. Every value is emitted through sprig's `toJson` (Go `encoding/json`): quotes, backslashes,
   newlines and other control characters, `<`, `>` and `&` are escaped and invalid UTF-8 becomes
   U+FFFD, so an environment value can only ever become one JSON string. For example
   `FIGCRAFT_BRAND_PRODUCT_NAME='Acme "Design" \ </script>'` renders as
   `"Acme \"Design\" \\ </script>"`.
3. The clients never trust the document. The SaaS shell (`webapp/saas/src/branding.ts`) and the
   editor (`apps/figcraft-web/src/branding.rs`) validate each field and fall back to that field's
   default when it is missing or invalid; a failed, slow (over 2 s, shell) or oversized (over
   16 KiB) response gives the defaults. Branding can never break the page.
4. The shell renders after `/branding.json` arrives (so a rebranded deployment does not flash the
   default name) and applies the document title and description, the `--brand-primary` and
   `--brand-accent` CSS custom properties, the header name and logo, the favicon and the footer.
   The editor at `/edit/` applies the document title and the loading/failure text; its in-canvas
   theme is not brandable yet (the editor UI crate exposes no accent hook). Its static
   `index.html` says only "Loading…", so no product name is baked into the image.

### Variables

All are optional. Empty or unset means the default, and the defaults reproduce the stock FigCraft
look exactly.

| Variable | Meaning | Default | Validation (else default) |
| --- | --- | --- | --- |
| `FIGCRAFT_BRAND_PRODUCT_NAME` | Product name: document title, header, editor title and loading text | `FigCraft` | Control and invisible format characters (C0/C1, bidi overrides, zero-width, line/paragraph separators) become spaces, whitespace runs fold to one space, trimmed, at most 64 characters (code points); empty after cleaning falls back |
| `FIGCRAFT_BRAND_SHORT_NAME` | Short name (`application-name` meta, compact places) | `FigCraft` | As above, at most 16 characters |
| `FIGCRAFT_BRAND_PRIMARY_COLOR` | Brand colour: logo mark, highlighted labels, link hover (`--brand-primary`) | `#b7df79` | Must match `^#[0-9a-fA-F]{6}$` (normalised to lower case) |
| `FIGCRAFT_BRAND_ACCENT_COLOR` | Accent: the background glow (`--brand-accent`) | `#94b85c` | Must match `^#[0-9a-fA-F]{6}$` |
| `FIGCRAFT_BRAND_LOGO_URL` | Header logo image (replaces the built-in mark; shown at 24 px high, at most 160 px wide) | none (built-in mark) | URL rule below |
| `FIGCRAFT_BRAND_FAVICON_URL` | Favicon | none (browser default) | URL rule below |
| `FIGCRAFT_BRAND_SUPPORT_URL` | "Support" footer link | none (link hidden) | URL rule below |
| `FIGCRAFT_BRAND_TERMS_URL` | "Terms" footer link | none (link hidden) | URL rule below |
| `FIGCRAFT_BRAND_PRIVACY_URL` | "Privacy" footer link | none (link hidden) | URL rule below |
| `FIGCRAFT_BRAND_FOOTER_TEXT` | Footer text (for example a copyright line) | none | Cleaned like the product name, at most 200 characters |

The footer is shown only when at least one footer link or the footer text is set.

**URL rule:** a same-origin absolute path starting with `/` (for example `/brand/logo.svg`), but
never `//` (scheme-relative), or an absolute `https://` URL with a host and no user name or
password. At most 2048 characters; whitespace, control characters and backslashes anywhere are
rejected. `http:`, `javascript:`, `data:` and relative paths (`brand/logo.svg`) are rejected.

The cloud service rejects unknown `FIGCRAFT_*` settings, so `FIGCRAFT_BRAND_*` belongs on the `web`
service only; `compose.yaml` passes it there and nowhere else.

### Custom brand assets

The web container serves `/srv/brand` at `/brand/*` with `Cache-Control: no-cache`,
`X-Content-Type-Options: nosniff` and a sandboxing `Content-Security-Policy` (a script inside an SVG
opened directly does not run). A missing file is a 404, never the app shell. Mount a directory
you own read-only by uncommenting the `volumes` entry of the `web` service in
`deploy/saas/compose.yaml`:

```yaml
    volumes:
      - ./brand:/srv/brand:ro
```

then, for example, `FIGCRAFT_BRAND_LOGO_URL=/brand/logo.svg` and
`FIGCRAFT_BRAND_FAVICON_URL=/brand/favicon.png`. Assets on another HTTPS origin work without the
mount.

### Example

```sh
FIGCRAFT_BRAND_PRODUCT_NAME="Acme Design"
FIGCRAFT_BRAND_SHORT_NAME=Acme
FIGCRAFT_BRAND_PRIMARY_COLOR="#2f6fed"
FIGCRAFT_BRAND_ACCENT_COLOR="#1b3f8a"
FIGCRAFT_BRAND_LOGO_URL=/brand/logo.svg
FIGCRAFT_BRAND_SUPPORT_URL=https://support.acme.example/
FIGCRAFT_BRAND_FOOTER_TEXT="© 2026 Acme Inc."
```

Quote colours in `.env` files so `#` is never read as a comment. Check the result with
`curl -s http://127.0.0.1:8080/branding.json`.

### Trademarks and assets

- The default name "FigCraft" and the default look belong to the FigCraft project. A rebranded
  deployment replaces them with the deployer's own name and assets.
- Deployers supply their own brand assets under their own rights. Never configure or ship a
  third-party product's logo, icon or name (Figma, Adobe and so on), and never add brand assets to
  this repository: the repository's asset rules (`CLAUDE.md`, `ATTRIBUTION.md`) still apply, and
  this feature adds no asset files.
- Branding changes presentation only. It does not change licences: FigCraft remains
  MIT OR Apache-2.0, and its notices (`LICENSE-MIT`, `LICENSE-APACHE`, `NOTICE`) still apply.

## Other configuration

The cloud service (`apps/figcraft-cloud`, parsed in `apps/figcraft-cloud/src/config.rs`) reads only
the settings below and rejects any other `FIGCRAFT_`, `DATABASE_`, `REDIS_`, `S3_`, `OIDC_`,
`SESSION_`, `EMBED_` or `OTEL_` variable. Secrets come from mounted `*_FILE` paths (at most 64 KiB);
production refuses inline secret values. Defaults and examples: `deploy/saas/env.example` and
`deploy/saas/compose.yaml`; operations: `deploy/saas/README.md` and
`docs/saas/deployment-operations.md`.

| Variable | Meaning | Values / default |
| --- | --- | --- |
| `FIGCRAFT_ENV` | Environment; production enforces HTTPS URLs and file-mounted secrets | `development` (default) or `production` |
| `FIGCRAFT_ROLE` | Which cloud role(s) this process runs | `all` (default), `api`, `relay`, `worker` |
| `FIGCRAFT_DEPLOYMENT` | Topology | `single` (default), `multi`, `embedded` |
| `FIGCRAFT_PUBLIC_URL` | Public base URL; required and HTTPS in production | e.g. `https://design.example.com` |
| `FIGCRAFT_SINGLE_TENANT_ID` | Tenant id of a single-tenant deployment | UUID |
| `DATABASE_URL_FILE` | PostgreSQL runtime URL (limited role) | mounted secret path |
| `REDIS_URL_FILE` (or `REDIS_URL`, development) | Redis URL | mounted secret path |
| `S3_ENDPOINT`, `S3_REGION`, `S3_BUCKET`, `S3_FORCE_PATH_STYLE` | S3-compatible object storage | `S3_FORCE_PATH_STYLE`: `true` or `false` |
| `S3_ACCESS_KEY_FILE`, `S3_SECRET_KEY_FILE` | Object-storage credentials | mounted secret paths |
| `OIDC_ISSUER`, `OIDC_CLIENT_ID`, `OIDC_AUDIENCE` | Identity provider; the issuer must be HTTPS in production | |
| `OIDC_CLIENT_SECRET_FILE` | OIDC client secret | mounted secret path |
| `SESSION_SIGNING_KEY_FILE` | Session signing key | mounted secret path |
| `EMBED_ISSUER_CONFIG_FILE` | Trusted embedding issuers | path to a TOML file |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Optional OTLP traces/metrics endpoint | URL |
| `RUST_LOG` | Log filter | `info` |

Compose-level settings (`deploy/saas/env.example`): `CADDY_IMAGE`, `POSTGRES_IMAGE`,
`POSTGRES_DATA_DIR`, `REDIS_IMAGE` and `SEAWEEDFS_IMAGE` pick the reviewed images
(`docs/saas/dependency-images.md`); the role-provisioning jobs take the administrator/migrator
files described in `deploy/saas/README.md`.

## Not yet brandable

- Request-time `<title>`/favicon in the HTML itself (the clients set them at runtime instead; the
  shell renders after branding loads, the editor's static page is neutral).
- The editor's in-canvas accent colour and logo (needs a public theme hook in the editor UI crate).
- Per-tenant branding (one brand per web deployment today).

# Core slice: two browser tabs co-editing one cloud document

The first end-to-end FigCraft Cloud path: the Compose stack (`deploy/saas`) serves the browser
editor, a share link opens one cloud document in it, and every tab that opens the link edits the
same document through the cloud collaboration relay, with confirmed edits durable in PostgreSQL and
S3 and surviving a restart of the `cloud` container.

## What serves what

| Path on the gateway (`http://127.0.0.1:8080`) | Served by | Notes |
|---|---|---|
| `/edit/` | `web` image, `/srv/edit/` | FigCraft editor (`apps/figcraft-web`, Rust → wasm32, built by trunk in `deploy/saas/web.Dockerfile`) |
| `/ws/v1/collab`, `/ws/v1/collab/*` | `cloud:8082` | Collaboration relay (the FigCraft collaboration protocol, the same one `figcraft-server` speaks) |
| `/api/v1/*`, other `/ws/v1/*` | `cloud:8081` | Cloud API |
| everything else | `web` image, `/srv/` | SaaS web app |

The editor is built with `trunk build --release --public-url /edit/`, so its HTML references
`/edit/figcraft-web-<hash>.js` and `/edit/figcraft-web-<hash>_bg.wasm` (content-hashed, cached
immutably; `index.html` is `no-cache`). `.wasm` is served as `application/wasm`. `/edit` redirects
to `/edit/`.

## Share links and `#join=`

A cloud share link is an ordinary FigCraft invite link (`crates/collab-net/src/invite.rs`):

```
ws://127.0.0.1:8080/ws/v1/collab?file=<document uuid>&token=<share secret>
```

The client connects to the endpoint `ws://127.0.0.1:8080/ws/v1/collab/` (the invite parser keeps
the path and appends `/`; no secret in the request) and sends `file` and `token` in its `Hello`.
The relay therefore sees the request path `/ws/v1/collab/`: the gateway forwards it unchanged and
the relay accepts the `/ws/v1/collab/` prefix (or the gateway strips it, whichever the relay
documents).

The browser URL wraps the link in the page fragment:

```
http://127.0.0.1:8080/edit/#join=<url-encoded invite link>
```

On startup the editor reads `location.hash`; `#join=<link>` is percent-decoded
(`apps/figcraft-web/src/hash.rs`, bounded and junk-tested) and joined through the same command the
File ▸ Join live session… menu uses (`collab.join {link}`). The fragment is then removed from the
address bar with `history.replaceState`, so the secret stays out of history and bookmarks; a
fragment never reaches a server, so it is not in access logs either. A link that can't be used shows
the normal join error (errors never echo the link or token) and an empty Join dialog.

## Run the demo

Needs Docker with Compose v2. The first build compiles the whole editor for wasm32 (several
minutes; BuildKit caches the cargo registry and target directory afterwards).

1. Secrets: create `deploy/saas/secrets/` with the files listed under `secrets:` in
   `deploy/saas/compose.yaml` (see `deploy/saas/README.md`; `deploy/saas/smoke-preflight.sh` shows
   how to generate disposable random values). Make them readable by UID 65532 (`chmod 644`).
2. Start the stack:

   ```sh
   cd deploy/saas
   docker compose --env-file env.example up --detach --build gateway
   ```

3. Create a document and a share link (single-tenant development mode; it creates the tenant,
   workspace, project and document when `--document` is omitted, and prints one line, the browser
   URL):

   ```sh
   docker compose --env-file env.example run --rm cloud --create-share-link --role editor
   # http://127.0.0.1:8080/edit/#join=ws%3A%2F%2F127.0.0.1%3A8080%2Fws%2Fv1%2Fcollab%3Ffile%3D…%26token%3D…
   ```

   `--document <uuid>` makes another link to an existing document; `--role viewer` makes a
   read-only one. The base URL comes from `FIGCRAFT_PUBLIC_URL`.
4. Open the printed URL in two browser tabs (or two browsers). Both join the document; draw in one
   and it appears in the other. The address bar shows `/edit/` without the fragment.
5. Durability: `docker compose --env-file env.example restart cloud`, then reload a tab with the
   same printed URL: the document comes back with every confirmed edit.
6. Clean up: `docker compose --env-file env.example down --volumes`.

The page must be served from `http://127.0.0.1:8080` (or `localhost`) or over HTTPS for WebGPU;
elsewhere over plain HTTP it falls back to WebGL2. Behind HTTPS the share link must use `wss://`
(set `FIGCRAFT_PUBLIC_URL=https://…`): browsers block `ws://` from secure pages.

## Checking the editor image alone

```sh
docker build -f deploy/saas/web.Dockerfile --build-arg CADDY_IMAGE=caddy:2.10.2-alpine -t figcraft-web:local .
docker run --rm -d -p 127.0.0.1::8080 --name figcraft-web-check figcraft-web:local
port=$(docker port figcraft-web-check 8080 | head -1 | cut -d: -f2)
curl -sI "http://127.0.0.1:$port/edit/"                  # 200 text/html, Cache-Control: no-cache
wasm=$(curl -s "http://127.0.0.1:$port/edit/" | grep -o '/edit/figcraft-web-[0-9a-f]*_bg.wasm' | head -1)
curl -sI -H 'Accept-Encoding: gzip' "http://127.0.0.1:$port$wasm"   # 200 application/wasm, immutable
docker rm -f figcraft-web-check
```

## Deferred

- Sign-in: the slice uses share-link secrets only; the SaaS web app has no "open in editor" button
  yet, and the editor has no cloud file browser (it opens what the link names).
- The editor's own File ▸ Save/Open still mean local downloads, not cloud documents.
- Presence names: browser guests show whatever name the live-session guest path gives them; there
  is no account identity yet.
- No service worker or offline queue: a tab that loses the relay behaves like any live-session
  guest that loses its host (reload the share link to rejoin).
- Precompressed (`.br`) assets; Caddy compresses on the fly.

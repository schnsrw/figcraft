# Session compatibility: cloud relay, figcraft-server, desktop host

FigCraft Cloud's collaboration relay (`crates/cloud-relay`, served by `apps/figcraft-cloud` on
port 8082 behind the gateway's `/ws/v1/collab`) is a new [`collab_net::hub::Backend`], not a new
protocol. It runs the same hub, transport, message set and collaboration core
(`figcraft_collab::Server`) as the self-hosted `figcraft-server` and the desktop app's session
host. The client side is shared too: the desktop app and the browser build join all three through
`figcraft_collab_net::NetLink` (native `Guest`, or the browser's WebSocket driver).

This page records what must stay compatible (S5-019), the tests that prove it, and the rule for
changing the protocol later.

## Versions on the wire

| Constant | Where | Value | What it guards |
|---|---|---|---|
| `WIRE_VERSION` | `crates/collab/src/message.rs` | `1` | First byte of every binary frame; then postcard. A frame of another wire version is never decoded as this one: the connection ends. |
| `PROTOCOL` | `crates/collab/src/message.rs` | `2` | Sent in `Hello.protocol` (and in `Manage`, `OpenLibrary`, `OpenPlugins`, `OpenHosting`, `Resolve`). Any other value is refused with `protocol N isn't supported (this server speaks 2)`. |
| `client_version` words | `message::client_version()` | `"<crate version> signals hosting"` | Optional capabilities. A server sends `Signal` only to clients that say `signals`, and `Hosting` / `Reported` only to clients that say `hosting`. |

## Link shapes

All three are read by `figcraft_collab_net::Invite::parse` (`crates/collab-net/src/invite.rs`).
The secret never travels in the WebSocket request: the client connects to the endpoint only and
sends the secret in its `Hello`.

| Server kind | Link it hands out | Endpoint the client connects to |
|---|---|---|
| Desktop host (`collab_net::host::Host`) | `figcraft://join?host=<ip:port>&file=<id>&token=<secret>` | `ws://<ip:port>/` |
| `figcraft-server` | the same `host=` form; behind a TLS proxy or path, `figcraft://join?server=wss://<host>/<path>/&file=…&token=…` | `ws(s)://<host>/<path>/` |
| Cloud relay | `ws(s)://<host>/ws/v1/collab?file=<document uuid>&token=<share secret>` | `ws(s)://<host>/ws/v1/collab/` (the gateway routes `/ws/v1/collab` and below; the hub answers on any path) |

Every form also round-trips as the same session's `ws(s)://…?file=…&token=…` URL. A cloud link
re-shared as a `figcraft://` link always takes the `server=` form (its path and TLS are kept); only
a plain `ws://` endpoint at `/` takes the short `host=` form, so a cloud link is never mistaken
for a desktop link. Errors about a link never echo it, its file id or its secret.

## Compatibility matrix

Rows are client builds, columns are server kinds; every cell speaks `PROTOCOL` 2 on `WIRE_VERSION` 1.

| Client | Desktop host | figcraft-server | Cloud relay |
|---|---|---|---|
| Desktop app, this build (`NetLink`/`Guest`, announces `signals hosting`) | Joins from `host=` link and `ws://` URL; edits reach the host | Joins from `host=`, `server=` and `ws://` forms; edits converge; restart: resumes as itself, offline edits go through | Joins from the cloud `ws://…/ws/v1/collab` invite and its `server=` re-share; edits committed before confirmation; relay restart: reconnects and catches up; viewer link read-only |
| Browser build, this build (WebSocket with an `Origin`, path = invite endpoint) | Joins and edits (raw-protocol probe with browser handshake) | Same hub as the desktop host | Same hub; the existing browser demo (CS-004) and `apps/figcraft-web` `#join=` parsing |
| Older build without `client_version` words | Welcomed; edits confirmed; never sent `Signal`, `Hosting` or `Reported` | Same hub rule | Same hub rule |
| A build speaking another `PROTOCOL` | Refused, naming both versions | Refused | Refused |
| Another `WIRE_VERSION` | Connection ends, never decoded | Same | Same |

## Tests (S5-019)

| What | Test |
|---|---|
| (1) `figcraft-server` in-process, temp data dir: join from all link shapes, edit, restart, catch-up | `apps/figcraft-server/tests/cloud_compat.rs` |
| (2) Desktop host: the app's client from both link shapes, a browser-style client, an older client, protocol and wire-version refusals | `crates/collab-net/tests/cloud_compat.rs` |
| (3) The same app client joins the cloud relay from a cloud invite (dev PostgreSQL + S3, fixture prefix f2) | `crates/cloud-relay/tests/cloud_compat.rs` |
| (4) The three link shapes round-trip; a cloud link is never a desktop link | `crates/collab-net/tests/cloud_compat.rs` |

```sh
cargo test -p figcraft-collab-net --test cloud_compat
cargo test -p figcraft-server --test cloud_compat
FIGCRAFT_TEST_DATABASE_URL_FILE=… FIGCRAFT_TEST_S3_DIR=… cargo test -p figcraft-cloud-relay --test cloud_compat
```

## Rule for future protocol changes

1. **Additive first.** A new server message, or a new field a client may ignore, is announced by
   the client with a new `client_version` word (like `signals` and `hosting`), and every server
   kind sends it only to clients that announce the word. Older clients keep working unchanged.
2. **A new client feature a server may lack** degrades: the client must work when the server never
   sends the new message (a desktop host or an older `figcraft-server` will not).
3. **A breaking change bumps `PROTOCOL`.** Servers refuse other versions by name, so a mismatch is
   a clear "update the app" refusal, never a misread. Because the desktop host, `figcraft-server`
   and the cloud relay share the hub, a bump lands in all three at once; the cloud deploys before
   clients that need it ship, and the desktop app keeps joining older servers until they upgrade.
   Accepting two protocol versions at once (if ever needed) is done in the hub's hello check, never
   in one backend only.
4. **`WIRE_VERSION` changes only with the encoding** (not with messages), and needs the same
   all-servers-first rollout.
5. **Link shapes are append-only.** `host=`, `server=` and `ws(s)://…?file=&token=` stay readable;
   a new shape must never parse as an existing one, and the secret never moves into the request.
6. Every change extends the three `cloud_compat.rs` tests above in the same commit.

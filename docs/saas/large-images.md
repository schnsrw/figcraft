# Large images in documents (TS-009, S5-018)

Status: step 1 (measurement). The design and its ADR follow in the commits of the same task; this
file starts with what the system does today, because the design is justified by it.

## How images travel today

An image is a document-level entry keyed by its content hash. The editor adds it
(`figcraft_format::add_image`, key = SHA-256 of the encoded file) and the next edit's diff carries
it as `Change::Put { entry: Entry::Image(hash), value: {width, height, bytes} }` with the file as
**base64 inside a JSON value**, which postcard then carries as a string. So an image costs 4/3 of
its size in every place a batch goes:

| Stage | Limit | What happens past it |
|---|---|---|
| Transport (`collab_net`, `RelayQuotas.max_message_bytes`) | 64 MiB per message | the connection is closed before the message is decoded |
| Collaboration server (`Server::receive`) | none on bytes (`Limits.max_message_bytes` is the decode bound) | accepts and orders the batch |
| Durable event (`events::encode_event`, `document_events.payload` CHECK) | 16 MiB (`MAX_EVENT_PAYLOAD_BYTES`) | `append_events` fails with `InvalidInput`, the commit fails, **the room is closed** |
| Replay page (`events::read_events`) | 32 MiB per page, always at least one event | fits: a 16 MiB event is a page of its own |
| Snapshot (`document_snapshots`, `MAX_SNAPSHOT_BYTES`) | 128 MiB | snapshot not stored (the events still hold the state) |
| Welcome to a late joiner | one message | the whole document, every image, in one message |

## Measured (DB-free, `crates/cloud-storage/tests/large_image_limits.rs`)

One image added by one client; sizes in MiB, run with `-- --nocapture`:

| image | batch on the wire | server accepts | event encode | stored state (snapshot + log) | late joiner's welcome |
|---:|---:|:--:|---|---:|---:|
| 0.1 | 0.1 | yes | ok (0.1) | 0.3 | 0.1 |
| 1 | 1.3 | yes | ok (1.3) | 2.7 | 1.3 |
| 5 | 6.7 | yes | ok (6.7) | 13.3 | 6.7 |
| 12 | 16.0 | yes | **PayloadSize** | 32.0 | 16.0 |
| 20 | 26.7 | yes | **PayloadSize** | 53.3 | 26.7 |
| 40 | 53.3 | yes | **PayloadSize** | 106.7 | 53.3 |

(49 MiB and above is over the 64 MiB transport limit: the message never reaches the server.)

## Failure modes, stated plainly

1. **Images of 12 MiB and over (16 MiB of base64 plus framing) are rejected, and the sender loops.**
   The collaboration server accepts the batch, but the relay cannot store it as an event
   (`MAX_EVENT_PAYLOAD_BYTES` is 16 MiB and nothing checks it before the server accepts), so the
   commit fails and the room is closed. Every client is dropped and reconnects; the author's client
   still holds the batch as pending, sends it again, and the cycle repeats. Nothing tells the user.
   `an_unstorable_batch_is_retried_for_ever` runs three rounds.
2. **Images of about 48 MiB and over close the connection** before the server decodes them, with the same
   pending-and-resend loop on reconnect.
3. **Late joiners download everything inline.** A client that joins (or reconnects after the log
   was trimmed) is sent every image of the file in the welcome, 4/3 of its size, in one message.
   A 40 MiB image is a 53 MiB welcome; three such images exceed the 64 MiB message bound on the
   receiving side's decoder.
4. **Stored state holds each image twice**: once in the document and once in the log of recent
   batches (`SessionState.log`, bounded by batch count, not bytes, in the cloud). A 40 MiB image is
   107 MiB of snapshot against the 128 MiB bound, so two or three large images stop snapshots
   from being stored (recovery then replays more events, each read whole).
5. **Memory.** Each hop holds the whole base64 string plus the decoded bytes: the relay's
   `Server` keeps the decoded image in the document and the encoded batch in the log.
6. Smaller images (up to about 5 MiB each) work end to end today; the cost is the 4/3 base64
   inflation, the doubling in stored state and the welcome size.

## What this means for the design

The cap is per message and per event, so the only fix that keeps the relay's invariants (one
bounded event per batch, bounded welcome, bounded snapshot) is to stop putting image bytes in
batches at all: batches carry a verified reference, bytes travel once over HTTP to object storage,
and clients fetch them lazily by hash. See the ADR sections below as they land.

## SAAS-ADR-017: images above a threshold travel as verified asset references

Status: accepted by the task owner's brief (2026-10-18). Implementation lands in TS-009 steps 2 to 6;
this section is updated with what each step actually does.

### Decision

1. **A reference, not bytes.** An image of at least `threshold` bytes (default 256 KiB, the server
   says what it uses) is put in the document as `Put { entry: Image(<sha256>), value:
   {"width":W,"height":H,"asset":{"len":N}} }`. The key is the content hash the document already
   uses; `len` is the file's size. A batch with a reference is a few hundred bytes whatever the
   image, so the event cap, the message cap, the welcome and the snapshot no longer depend on
   image sizes. Only keys that are a lower-case hex SHA-256 can be references; other keys (the
   colour picker's legacy scheme) and images under the threshold stay inline.
2. **Bytes go over HTTP to object storage, content-addressed per tenant**, authorised by the same
   credential as the room (a session, a realtime ticket or a share link), through new document
   routes that reuse the existing asset machinery (`assets::store_asset_bytes`, `attach_asset`).
   Per-image cap configurable (default 100 MiB); a per-tenant quota hook. Each upload is one
   request (no resumable protocol: the existing machinery has only single presigned PUTs; chunking
   is recorded as follow-up work).
3. **The relay verifies before it accepts.** A batch that references a hash is accepted only if
   the tenant holds a verified asset with that hash and that size
   (`AssetDirectory::verified`), so one tenant cannot reference another's bytes and a reference
   cannot outrun its upload. Replay of stored events does not re-check (it was checked once, and
   a document's reference keeps the asset alive).
4. **Negotiated capability, same pattern as the drag signal.** A client announces `imagerefs` in its
   hello `client_version`. A server that holds assets answers that client with an `assets` signal
   carrying its policy (`<threshold>,<max_bytes>`). Only then does the client send references.
   * Old client, new relay: never told, never sent a reference (batches, delta catch-ups and
     snapshots are filtered), keeps sending inline images; the relay still accepts them. New:
     inline batches the event log cannot store (over 15 MiB encoded) are now refused with a reason
     instead of being accepted and lost, so the retry loop of failure mode 1 ends for old clients too.
   * New client, old server (the filesystem server, a desktop host, an older relay): no signal, so
     the new client falls back to inline images within the old limits.
5. **Lazy fetch with a placeholder.** A client that applies a reference holds a placeholder (an
   image with no bytes). It fetches by hash through an authorised GET, verifies the bytes hash to
   the key, caches by hash, and shows a placeholder and retries with backoff while the bytes are
   missing. Range or streaming reads are not needed.
6. **Garbage collection.** Uploading attaches the asset to the document through the existing
   `object_references` rows (`owner_kind = document`), so an asset is never collected while its
   document exists. An image the document stops using keeps its reference (the merge already keeps
   unused images; file size only).
7. **Snapshots and events carry references**, so the welcome, the snapshot and replay stay small;
   the stored document holds placeholders, not bytes.

### Consequences and limits

* A reference's `len` is checked when it is accepted. State sent from a server's snapshot carries
  `len: 0` (a placeholder holds no size); the bytes' hash is the real check on fetch.
* A file exported from a cloud document includes only images the exporting client holds (all it
  has fetched). A client that exports before the fetch finished has placeholders in the export.
* An undo that re-adds an image removed earlier restores it from the client's own bytes inline
  (the previous behaviour); over 15 MiB the relay refuses it with a reason. Documents rarely
  remove images (the merge and the editor keep them).
* Not covered: images embedded in the published library, which still move inline.

## What each step built

### Step 3: the HTTP routes (`apps/figcraft-cloud/src/api/images_api.rs`, `openapi/images-api-v1.json`)

* `PUT /api/v1/documents/{document_id}/images/{sha256}` (body: the file) and `GET` the same path.
  The upload checks the credential first, then the address, then the declared size, then reads the
  body (bounded again while reading), hashes it (off the async threads above 256 KiB) and compares
  it with the address; storage then validates it as a PNG, JPEG, WebP or GIF within the pixel
  budgets (`assets::store_asset_bytes_with_limit`, the existing content-addressed, deduplicating,
  read-back-verified path), attaches it to the document (`assets::attach_asset`, an
  `object_references` row of kind document, so garbage collection keeps it) and commits.
  `201` new bytes, `200` bytes the tenant already held.
* Credentials: the browser session cookie (same-origin `fetch`), a service account, or the room's
  own token in `X-Figcraft-Room-Token` (a share-link secret; a realtime ticket works for the minute
  it lives). A viewer reads but cannot add (403); a document the credential does not reach is 404.
* A read returns bytes only for an image attached to the requested document, so knowing a hash
  does not let a reader of one document read another document's images, and another tenant's
  identical bytes are a different object (`find_document_asset` filters by tenant, hash and
  attachment).
* Limits: `FIGCRAFT_IMAGE_MAX_BYTES` (default 100 MiB, up to 256 MiB), refused from `Content-Length`
  before the body is read; `FIGCRAFT_IMAGE_TENANT_QUOTA_BYTES` (new bytes only; reused bytes are
  free; 507); `FIGCRAFT_IMAGE_UPLOAD_SECONDS`; at most 8 transfers at once per replica (503 and
  retry). Only the image `PUT` is exempt from the 1 MiB body cap and 30 s deadline every other
  route has. The size limit that was 32 MiB for uploads in the storage layer is a parameter now
  (`prepare_asset_with_limit`), default unchanged for other callers.
* Not built: resumable or chunked upload (the object-store layer only has single presigned PUTs;
  each image is one request, held in memory at the API); direct-to-S3 upload from the browser
  (needs bucket CORS); a conditional GET (clients cache by hash themselves).
* Evidence (no database): `images_api_tests.rs` runs the real router and middleware with an
  in-memory gate: a 30 MiB image up and down, role and credential matrix, tenant isolation, per
  document attachment, hash mismatch, type and size limits, quota, transfer bound, the OpenAPI
  document; `assets.rs` unit tests for the raised cap and the address parser. The production
  gate's SQL (`find_document_asset`, `read_document_asset`, `tenant_asset_bytes`) is exercised
  only against a live database (not run in this task; see the tracker).

### Step 4: the relay checks references; recovery carries them

* `CloudBackend` gives every room's collaboration `Server` an `AssetDirectory`
  (`crates/cloud-relay/src/image_assets.rs`): a reference is accepted only if the room's tenant
  holds that verified asset with exactly that size (`assets::claim_document_asset`: tenant, hash and
  size, in a tenant transaction, so another tenant's identical bytes are invisible), and the same
  transaction attaches it to the document, so a reference uploaded for document A and used in
  document B keeps the asset alive for B too. Storage failure or a timeout refuses the batch with a
  reason (`wasn't uploaded to this file`); the client's batch is not retried blindly.
  `CloudBackendConfig::image_refs` (on by default, the policy the app derives from
  `FIGCRAFT_IMAGE_MAX_BYTES`) and `max_batch_bytes` (15 MiB, a megabyte under the event cap) are
  set on every room the backend opens, on the synchronous and the asynchronous open path.
* Inline images from older clients are still accepted, up to 15 MiB of encoded batch; above that
  the client is told why, once, and its batch leaves the queue (measured loop: gone).
* Recovery: snapshots and events hold references (`asset: {len}`) and placeholders, and replay
  does not consult the directory. Measured after (`crates/cloud-storage/tests/large_image_refs.rs`,
  no database), for an image of 100 KiB up to 100 MiB: the batch is 124 to 127 bytes, the stored
  event 124 to 127, the stored state about 570, a late joiner's welcome about 460; the figures do
  not depend on the image. A room restored from its snapshot with the later events replayed (empty
  directory) tells a joiner exactly which two 30 and 45 MiB images it has to fetch. An older client
  in the same room is sent the batch without the reference and keeps pasting inline.
* Not run here: the directory's SQL against a live database (compile-checked only).

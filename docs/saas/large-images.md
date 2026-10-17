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

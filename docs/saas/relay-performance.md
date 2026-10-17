# Relay performance (S5-024, S5-025)

Status: first measurements. One relay process, small scale, on a development machine with
unrelated load. These are the first measured numbers for the cloud relay and they replace guesses.
Production sizing still needs a quiet host, multiple replicas and larger runs (see "What is not
covered yet").

The 2026-10-17 figures below precede the S5-025 recovery change that transfers the already
restored `Server` from `DurableRoom` into the live hub. The former path serialized that server and
called `Server::restore` a second time. The change removes one restore pass; its latency effect
still needs measurement with the same harness before changing the verdicts or sizing figures.

## Harness

`crates/cloud-relay/examples/relay_load.rs` starts a real `collab_net` hub serving `CloudBackend`
on loopback against the dev PostgreSQL and S3. It creates documents and editor share links in the
fixture tenant (UUID prefix `f1`) through `cloud-storage` in tenant transactions. It then drives
simulated editors: raw tungstenite clients that speak the collab protocol (hello, then
property-change batches). Phases (`--phases steady,storm,large,recovery`, all by default):

1. **steady:** N editors across M documents. Each editor sends one rename batch every `1/rate`
   seconds and times it from send until its own confirmation arrives. The relay confirms only
   after the PostgreSQL commit, so this is commit latency. Slow readers join and never read.
2. **storm:** K editors drop at once, then reconnect together behind a barrier. The harness
   measures connect to welcome, and the first commit after reconnecting.
3. **large:** seed a document with many frames (2000 creates per batch), leave (the room commits
   and snapshots), cold-open it, edit it, and join it warm.
4. **recovery:** cold opens (hello to welcome on a new hub) of documents with an S-node snapshot
   and E events after it. The events are left behind by a relay that "died" before it
   snapshotted: its hub is kept alive but never used again, so it never writes a snapshot.

The harness also reports process RSS (from `ps`), the `document_events` rows and bytes written by
the fixture tenant (exact: `sum(pg_column_size(row))`), snapshot rows, the `document_events`
relation size and WAL position deltas (both shared with other writers), and outbound bytes (every
message the editors received). `--timeout S` sets the backend's storage timeout (15 s by default,
the relay's default; recovery runs inside it). `--cpu-nodes N` instead times the collaboration
core alone in process: seed, `Server::state`, `Server::restore`, join, and welcome encoding.
Relay warnings and errors go to stderr, so a refused hello says why.

### Commands

```sh
export FIGCRAFT_TEST_DATABASE_URL_FILE=…/devdb/host_database_url \
  FIGCRAFT_TEST_S3_DIR=…/devdb \
  FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE=…/devdb/admin_database_url
cargo build --release -p figcraft-cloud-relay --example relay_load
R=$CARGO_TARGET_DIR/release/examples/relay_load

# Run A: every phase, relay defaults.
$R --editors 20 --documents 5 --seconds 120 --rate 2 --slow 2 --storm 20 \
  --large-nodes 20000 --large-commits 50 \
  --recovery-nodes 20000 --recovery-events 500 --recovery-runs 3
# Run B: run A stopped at the large phase (see below); the rest again.
$R --documents 1 --phases large,recovery --large-nodes 20000 --large-commits 50 \
  --recovery-nodes 20000 --recovery-events 500 --recovery-runs 3
# Run C: the large phase with a 60 s timeout, then the core alone.
$R --documents 1 --phases large --large-nodes 20000 --large-commits 50 --timeout 60
$R --cpu-nodes 20000
```

The admin URL is optional; without it the database figures are skipped (row-level security hides
the byte sizes from the runtime role).

### Machine

- Apple M4, 10 cores, 16 GiB, macOS 26.3, release build.
- PostgreSQL 18.6 and SeaweedFS (S3) in local Docker containers, reached over loopback.
- **The host was shared with unrelated parallel builds.** Load average (1 min) was 37 at the
  start of run A, 21 to 30 during run B and 4 to 5 during run C, on 10 cores. A first run earlier
  the same day at load 100 to 266 is kept for comparison. Treat the latencies as pessimistic.
- Clients run in the same process as the relay. They keep no replicas, but their threads and
  buffers are counted in RSS.

## Results (2026-10-17)

### Steady: 20 editors, 5 documents, 120 s, 2 batches/s each, 2 slow readers (run A)

| measure | n | p50 ms | p95 ms | p99 ms | max ms |
|---|---|---|---|---|---|
| commit latency (send to confirmation) | 4800 | 4.4 | 37.6 | 52.2 | 95.6 |
| welcome (connect to welcome, first joins) | 20 | 2.3 | 34.3 | 87.6 | 87.6 |

The first run, at load 100+, measured commit latency p50 8.9 ms, p95 111.5 ms, p99 214.8 ms.

- 4800 of 4800 batches confirmed (39.8 commits/s, equal to the offered load), no refusals, no
  editor failures.
- Outbound: 389,822 B to editors, which is 162 B/s per editor or 0.6 MiB per editor-hour (4
  editors per document, each receiving everyone's renames).
- RSS: 8.1 MiB at start, 9.1 MiB once the hub was up, 20.8 MiB with 5 documents open and 20
  editors joined. That is about **2.3 MiB per open small document**, including the clients'
  threads. RSS drifted by +2.6 MiB over the 2 minutes.
- `document_events`: +4800 rows and +729,600 B of row data, which is **152 B per row**. At 2
  batches/s that is **7,172 rows and 1.04 MiB of row data per editor-hour**. Over the same window
  the relation (including its indexes, and with other writers present) grew 1.66 MB (about 350 B
  per row as an upper bound), and the database's WAL grew 4.1 MB (34 kB/s, an upper bound of
  about 860 B of WAL per event).
- Snapshots: 5 (one first-state snapshot per new document, 1,651 B in all).
- Slow readers: the hub dropped both slow readers between 45 s and 50 s (22 connections fell to
  20), after about 1024 messages had queued for them (`NetLimits::queue`). The other editors kept
  committing. The first run saw the same 45 to 50 s eviction.

### Reconnect storm: 20 editors dropped and reconnected at once (run A)

| measure | n | p50 ms | p95 ms | p99 ms | max ms |
|---|---|---|---|---|---|
| reconnect to welcome | 20 | 194.6 | 194.8 | 196.2 | 196.2 |
| first commit after reconnect | 20 | 20.7 | 20.8 | 20.8 | 20.8 |

All 20 editors were back and had a commit confirmed within 221 ms, with no failures (the first run:
100 ms). Dropping every editor emptied every room, so the storm also covered 5 room closes (commit
and snapshot) and 5 cold reopens of small documents.

### Large document: 20,000 frames

| measure | run | n | p50 ms | p95 ms | p99 ms | max ms |
|---|---|---|---|---|---|---|
| seeding commit (2000 creates per batch) | C | 10 | 349.7 | 881.7 | 881.7 | 881.7 |
| seeding commit (2000 creates per batch) | B | 10 | 1292.5 | 3353.9 | 3353.9 | 3353.9 |
| rename commit on the large document | C | 50 | 1.2 | 3.6 | 9.4 | 9.4 |

- **With the relay's default 15 s storage timeout the cold open failed twice (runs A and B):**
  `recovery timed out`, then the hello was refused with "this document can't be opened right
  now". Run A stopped there; the harness now reports a refused cold open instead of stopping.
- With a 60 s timeout on a quieter host (run C) the cold open (claim, snapshot restore, welcome)
  took **8.5 s**; the first run took 10.5 s. The welcome is 616,366 B. A warm join of the open
  document takes 12 ms.
- RSS rose by 82 MiB when the document opened (102 MiB to 184 MiB): about **4 KiB per node**.

### Collaboration core alone (`--cpu-nodes`, no network or storage)

| frames | run | seed | `state()` | `restore()` | join | encode welcome | welcome bytes |
|---|---|---|---|---|---|---|---|
| 20,000 | C (load 5) | 4,149 ms (10 batches) | 9 ms | 4,093 ms | 6 ms | 3 ms | 616,366 |
| 20,000 | first (load 100+) | 14,012 ms (10 batches) | 13 ms | 5,410 ms | 9 ms | 3 ms | 616,366 |
| 4,000 | first (load 100+) | 938 ms (2 batches) | 26 ms | 1,237 ms | 14 ms | 3 ms | 120,366 |

- `Server::restore` accounts for nearly all of a cold open: run C's 8.5 s cold open is two 4.1 s
  restores.
- `CloudBackend::recover` restores the server twice: `DurableRoom::recover` restores the
  snapshot, then the backend calls `state()` and `Server::restore` again
  (`crates/cloud-relay/src/collab_backend.rs`, `recover`). Restoring once would halve a large
  document's cold open.
- Applying large create batches is superlinear in the document size: 10 batches of 2000 take 4 s
  in process, against 0.9 s for 2.

### Room recovery: cold open of a document, hello to welcome (run B, 3 runs each, 15 s timeout)

| snapshot nodes | events after it | n | refused | p50 ms | p95 ms | p99 ms | max ms |
|---|---|---|---|---|---|---|---|
| 0 | 500 | 3 | 0 | 44.4 | 65.6 | 65.6 | 65.6 |
| 20,000 | 0 | 3 | 0 | 8,523.7 | 8,648.8 | 8,648.8 | 8,648.8 |
| 20,000 | 500 | 3 | 0 | 8,323.3 | 14,295.5 | 14,295.5 | 14,295.5 |

Replaying 500 events costs tens of milliseconds; the snapshot restore is the whole cost. The p95
of 14.3 s with 500 events is one slow open on a loaded host, 0.7 s short of the timeout.

## Against the targets

Targets come from `docs/saas/product-requirements.md` ("Non-functional requirements"). The
roadmap's S5 exit asks that room recovery meets the initial target for supported document sizes.

| target | measured | verdict |
|---|---|---|
| Relay p95 server processing under 50 ms (no client network, no snapshot loading) | commit p95 37.6 ms end to end on loopback (p50 4.4 ms, p99 52.2 ms), including the PostgreSQL commit, at load 37 | met at this scale on this host (111.5 ms p95 at load 100+). Re-measure on a quiet host and at production scale |
| Room recovery p95 under 10 s within the configured size limit | small documents with 500 events: p95 66 ms. 20k-frame snapshot: p50 8.3 to 8.5 s, p95 8.6 to 14.3 s, and 2 of 2 in-process cold opens timed out at 15 s under load | met for small documents; not met for 20k frames. Restoring once is the first fix; the default storage timeout must exceed the recovery time of the largest supported document |
| No acknowledged-edit loss | 4800 of 4800 confirmed batches are in `document_events` (rows counted per tenant) | holds for this run |

## Sizing from measurements

No relay sizing existed before these measurements; this replaces the guesses. Every figure is
from the runs above. Assumptions are stated and are deliberately conservative.

- **Memory per relay process:** about 9 MiB base, plus about 2.3 MiB per open small document,
  plus about 4 KiB per node of an open document. A relay holding 200 small documents and five
  20k-node documents needs about 9 + 460 + 400 ≈ 870 MiB. Request 1 GiB and limit 2 GiB per
  replica; `max_open` (1024 by default) needs a matching memory budget, or a lower value.
- **Commit throughput per relay process:** the hub runs storage calls on its own thread, so the
  document commits in a flush are serial. At 4 to 9 ms p50 per commit, plan on about **100
  commits/s per relay replica**. Several batches of the same document can share one commit, so
  under load this is a floor, not a ceiling. At the measured 2 batches/s per active editor that is
  about 50 continuously typing editors per replica. Real editors send far fewer batches (drags and
  typing are coalesced): at an assumed 0.2 batches/s it is about 500 active editors per replica.
  Scale on commit latency p95 and active rooms (`deployment-operations.md`, "Scaling
  thresholds").
- **Recovery is serial too:** a cold open runs on the hub thread, so while a 20k-frame document
  restores (8 s here) every other room on that relay waits for its next flush. Until restore is
  faster or moved off the hub thread, keep large documents spread across replicas and alert on
  recovery time.
- **PostgreSQL:** each batch is one `document_events` row of about 152 B of row data, about
  350 B with indexes and up to about 860 B of WAL. At 2 batches/s that is about 1 MiB of rows,
  about 2.4 MiB of table and index, and up to about 6 MiB of WAL per editor-hour. Snapshots bound
  the replay cost, not the rows, so event retention is what bounds the table.
- **Outbound bandwidth:** about 160 B/s per editor for small property edits with 4 people per
  document. This grows linearly with the number of people in a room. Welcomes dominate on joins:
  a 20k-frame document sends 616 kB per join, and a reconnect storm of K editors on large
  documents sends K × welcome size at once.
- **Reconnect storms:** 20 simultaneous reconnects finished in 100 to 221 ms for small documents.
  For large documents the cold-open time per room (8 to 10 s at 20k frames, serial on the hub
  thread) dominates. A relay restart with many large rooms open needs staggered reconnects or a
  faster restore.
- **Slow readers:** a non-reading client is disconnected after about `NetLimits::queue` (1024)
  queued messages. At 40 room messages/s that took 45 to 50 s. Lower `queue` or
  `max_queued_bytes` for faster eviction.

## What is not covered yet

- **Multiple relay replicas** (the S5 leases and routing are not built yet). Every number here is
  one process.
- **Production scale and a quiet host:** hundreds of documents, thousands of editors, PostgreSQL
  on its own host, and a run long enough to see snapshot scheduling.
- **Separate client processes,** so RSS counts the relay alone.
- **The maximum supported document size:** 20k frames was measured; the 5M-node `Limits` maximum
  was not, and at the measured restore cost (about 0.2 ms per node) it cannot meet the recovery
  target. A supported size limit has to be declared from these numbers.

## Revision history

- 2026-10-17: removed the second `Server::restore` during `CloudBackend` recovery. The existing
  measurements remain the baseline until a comparable run is recorded.
- 2026-10-17: first harness and measurements (S5-024 and S5-025, partial). Phase selection,
  `--timeout`, relay logs and non-fatal cold-open refusals added after the default 15 s timeout
  refused a 20k-frame cold open.

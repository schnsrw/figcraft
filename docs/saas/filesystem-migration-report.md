# Filesystem server migration report

> **Status:** format 1 · **Last updated:** 2026-10-17 · **Tracker:** S2-021

Before a `figcraft-server` data directory is imported into FigCraft Cloud (PostgreSQL metadata and
events, S3-compatible snapshot and library objects), an operator inventories it with
`figcraft_cloud_storage::legacy_scan::scan_data_directory`. The scan produces a
`MigrationReport`. This page explains the report's fields, what blocks migration, and how the
import will use the report.

## Scanner guarantees

- **Read-only.** Files are only opened for reading. Nothing in the tree is written, renamed,
  locked or deleted. Leftover `*.tmp` files, which the server deletes when it starts, are
  reported and left in place.
- **No symlink following.** A symbolic link anywhere under the root is reported as
  `symlink_skipped` and is not opened. Directory entries are typed with `lstat` semantics, and an
  opened file is checked again through its handle.
- **Fixed depth.** The scanner descends only the known layout (`files/<id>/…` and `plugins/…`),
  so it reads at most two levels below the root. Unexpected directories are reported as entries.
  Their contents are not read.
- **Bounded.** `ScanLimits` caps documents, entries per directory listing, snapshot bytes, log
  bytes, log records, JSON bytes, plugin bundle bytes and problems kept. The scanner streams logs
  one record at a time and never allocates a record larger than the bytes left in the file.
- **Never crashes.** Damaged or hostile input becomes a problem code. Unit tests run a sweep over
  every truncation, every single-byte corruption and pseudo-random garbage.
- **No secrets.** `server.key` is reported by presence only. The report omits link token hashes,
  join-code hashes, owner token hashes, recorded client addresses and blocked addresses. Problems
  carry fixed codes and byte offsets, never decoder messages that could quote document content.
- **Stop the server first.** The scanner can't tell whether a server is running: finding out
  would mean taking the server's `LOCK`, and the scanner takes no locks. The server keeps `LOCK`
  and `server.json` after it stops, so `lock_present` and `runtime_file_present` show only that
  these files exist. Scan a stopped server's directory or a copy of it.

## On-disk layout the scanner understands

| Path | Parsed as |
|---|---|
| `files/<id>/meta.json` | JSON: `name`, `created`, `updated`, `seq`, `opened`, optional `library {id,name,version}`, `owner`, `ip` |
| `files/<id>/links.json` | JSON array: `id`, `access` (`edit`/`view`), `label`, `created`, `hash`, optional `code` |
| `files/<id>/snapshot.bin` | `FCSN`, format `1`, generation u64 LE, CRC-32 u32 LE, then a wire-encoded `SessionState` |
| `files/<id>/log.bin` | `FCLG`, format `1`, generation u64 LE, then records: length u32 LE, CRC-32 u32 LE, one wire-encoded `Journal` |
| `files/<id>/library-<n>.bin`, `library.bin` | Size only (**unparsed**: `.figcraft` archives) |
| `files/<id>/usage.json` | JSON array; entries are counted |
| `plugins/index.json` | JSON array of `PluginEntry`; each bundle `plugins/<sha256>.bin` is checked by size and SHA-256 (bundle contents are **unparsed**) |
| `blocked.json`, `reports.json` | JSON arrays; entries are counted |
| `LOCK`, `server.json`, `server.key` | Presence only |

A `<id>` is 1–64 ASCII letters, digits, `-` or `_`, as the server requires. Two things are not
verified: team library archives (`LibraryFile.parsed` is always `false` in format 1) and whether
journal entries replay cleanly into the session's document. The scanner checks the sequence order
that replay depends on. Replaying the changes themselves is left to the import.

## Report fields

`MigrationReport` serializes with serde (JSON in practice). Field names are `snake_case`.
Optional fields are omitted when absent.

| Field | Meaning |
|---|---|
| `format_version` | `1`. Bumped only for changes a consumer must notice; new optional fields and new problem codes are additive |
| `scanned_at` | Seconds since 1970 UTC, supplied by the caller (the scanner reads no clock) |
| `root` | The scanned directory as given |
| `complete` | No count cap was reached: every document, record and problem is in the report |
| `migratable` | `complete` and no `blocking` problem |
| `totals` | Document counts by status, snapshot/log/library/plugin bytes, `replayable_batches`, problem counts by severity, `problems_dropped` |
| `server` | `lock_present`, `runtime_file_present`, `key_present`, `blocked_addresses` and `abuse_reports` counts |
| `documents[]` | One entry per valid `files/<id>` folder, sorted by id |
| `plugins` | Present when `plugins/` exists: `index_present` and per-entry `id`, `version`, `bytes`, `sha256`, `bundle_present`, `bundle_valid` |
| `problems[]` | Findings in scan order: `code`, `severity`, `path` (relative, `/`-separated), optional `document` and `offset` |

Each `documents[]` entry has these fields:

| Field | Meaning |
|---|---|
| `id`, `path` | Folder name and `files/<id>` |
| `status` | `ready` (info only), `degraded` (warnings: data the server itself drops) or `blocked` |
| `meta` | `name`, timestamps, `seq`, `published_without_key`, `origin_address_recorded`, `library` |
| `links` | `edit`/`view`/`with_join_code` counts and each link's `id`, `access`, `label`, `created`, `has_join_code` |
| `snapshot` | `bytes`, `generation`, `checksum_valid`, `decoded`, `session_seq`, `changes`, `retained_batches`, `issued_clients`, `fault` |
| `log` | `bytes`, `generation`, `matches_snapshot`, `records`, `issued`/`accepted`/`refused`, `already_applied`, `replayable`, `first_accepted_seq`, `last_accepted_seq`, `last_replayable_seq`, `sequence_break_at`, `damage {code, offset, trailing_bytes}` |
| `last_accepted_seq` | The sequence the document reaches: the snapshot's `session_seq`, advanced by the replayable batches of a log that continues it |
| `libraries[]` | `name`, `version` (none for `library.bin`), `bytes`, `parsed` |
| `usage_reports` | Entries in `usage.json` |
| `bytes` | Sum of the folder's regular files, unknown ones included |

The `log` counts follow the server's replay rule. Batches at or below the reached sequence are
`already_applied`. Each batch that is exactly one more is `replayable`. At the first gap the
server stops, which the report marks with `sequence_break_at`. The server also stops at the first
damaged record, which the report marks with `damage`: `trailing_bytes` are the bytes the server
drops. Some logs come from before an interrupted compaction (`matches_snapshot: false`). The
server ignores those logs because the snapshot already holds their contents.

## Problem codes

Each code has a fixed severity (`ProblemCode::severity`). Codes are never renamed or reused.

| Severity | Codes |
|---|---|
| `blocking` | `root_unreadable`, `root_not_directory`, `directory_unreadable`, `directory_entry_limit_reached`, `document_limit_reached`, `problem_limit_reached`, `file_too_large`, `read_failed`, `meta_missing`, `meta_unreadable`, `links_missing`, `links_unreadable`, `snapshot_missing`, `snapshot_bad_header`, `snapshot_unsupported_format`, `snapshot_truncated`, `snapshot_checksum_mismatch`, `snapshot_undecodable`, `log_unsupported_format`, `log_entry_limit_reached` |
| `warning` | `invalid_document_id`, `symlink_skipped`, `unknown_entry`, `log_missing`, `log_bad_header`, `log_truncated_header`, `log_truncated_record`, `log_checksum_mismatch`, `log_undecodable_record`, `log_sequence_break`, `library_missing`, `usage_unreadable`, `server_list_unreadable`, `plugin_index_unreadable`, `plugin_entry_invalid`, `plugin_bundle_missing`, `plugin_bundle_mismatch` |
| `info` | `files_directory_missing`, `leftover_temp_file`, `log_stale_generation` |

These rules decide what blocks migration:

- **Blocking** problems mean the folder has nothing trustworthy to import: no readable metadata,
  links or snapshot. They also cover a newer on-disk format this scanner doesn't read, and an
  inventory a cap left incomplete. The server itself skips most of these folders at startup.
- **Warning** problems match losses the filesystem server already accepts when it opens the file,
  such as a torn log tail, a log it would restart, or a skipped folder or bundle. Importing a
  degraded document reproduces what the server would serve. The operator should still review
  warnings before importing.
- **Info** problems need no action.

## How the S3 import consumes the report

The import is a later milestone. This is the contract it will follow:

1. Refuse to start unless `format_version` is known and `complete` is `true`.
2. Refuse to start when `migratable` is `false`, unless the operator gives an explicit list of
   blocked document ids to exclude. Every exclusion is written to the audit log.
3. For each `ready` or `degraded` document:
   1. Re-read the source folder with the same parser.
   2. Upload the snapshot body as an object and register it (S2-017).
   3. Re-encode exactly `log.replayable` `Journal::Accepted` entries as `document_events`
      (S2-013 to S2-015). The import starts each document at `snapshot.session_seq` and must end at
      `last_accepted_seq`. Each mismatch fails that document.
   4. Upload each library file as an object, which the import parses and validates itself.
   5. Recreate links from `links.json`: token hashes are carried over, never tokens.
4. Import plugin bundles only when `bundle_valid` is `true`.
5. Compare the import's results with the report's `totals` (documents, `replayable_batches`,
   bytes) and keep both artifacts with the migration record.

Client-id issue and refusal entries (`issued`, `refused`) are session state, not events, so the
import does not copy them (see S2-013).

## Dry-run migration plan

> **Status:** plan format 1 · **Tracker:** S3-017

`figcraft_cloud_storage::migration_plan::plan_migration(&report, &target)` turns a
`MigrationReport` and a `PlanTarget` into a `MigrationPlan`. The target is chosen by the
operator: `tenant_id`, `workspace_id`, `project_id`, the `principal_id` recorded as the actor of
imported events, and the `bucket`. The plan lists what the import (S3-018) would create. It is a
dry run: the planner reads no files, clock, randomness or database and writes nothing.

`plan_migration` returns an error for a report `format_version` it doesn't read
(`UnsupportedReportFormat`) and for a bucket that isn't a valid S3 name (`InvalidBucket`). Any
other input yields a plan, with exclusions, deferrals and blockers where the input is unusable.
A report deserialized from hand-edited JSON can't make it panic, and every list is capped:
100,000 documents, 256 library files per document and 10,000 plugin entries.

### Deterministic identifiers

Every identifier comes from the tenant and the legacy identifiers, so a rerun plans the same
rows and objects and the import can find what an earlier attempt created. The workspace's `uuid`
dependency doesn't enable UUIDv5 (SHA-1), so the planner uses its own derivation: the first 16
bytes of SHA-256 over `figcraft-migration-plan/v1`, the tenant id and each part. Each part is
length-prefixed (u64 LE), and the result is stamped as an RFC 9562 version 8 (custom) UUID with
the RFC 4122 variant. The tenant is included because `documents.id` and `objects.id` are global
primary keys.

| Resource | Parts |
|---|---|
| Document (`documents.id`, mapping `internal_id`) | `document`, legacy id |
| Snapshot row (`document_snapshots.id`) | `document_snapshot`, legacy id, snapshot seq |
| Snapshot object | `snapshot_object`, legacy id, snapshot seq |
| Library (key path) | `library`, legacy id |
| Library object | `library_object`, legacy id, file version |
| Plugin (key path) | `plugin`, plugin id |
| Plugin object | `plugin_object`, plugin id, version text, SHA-256 hex |

Changing the derivation changes every identifier, so it requires a new plan format version.

### Plan fields

Field names are `snake_case`, and optional fields are omitted when absent. Lists are sorted:
documents and exclusions by legacy id, plugins by id and version, deferrals by source. The JSON
layout is pinned by `crates/cloud-storage/tests/migration_plan_golden.json`.

| Field | Meaning |
|---|---|
| `format_version` | `1` |
| `report_format_version`, `report_scanned_at` | Copied from the report |
| `target` | The `PlanTarget` as given |
| `executable` | `blockers` is empty |
| `blockers[]` | `report_incomplete`, `blocking_server_problem` (a blocking problem not tied to one document), `exclusion_approval_required` (some documents are excluded; the import needs the operator's explicit list), `document_limit_reached` |
| `totals` | `documents`, `excluded`, `deferred`, `degraded`, `events`, `links`, `plugins`, `objects`, `snapshot_bytes`, `library_bytes`, `plugin_bytes`, `upload_bytes` (sum of planned object sizes), `event_bytes_max` (sum of the source logs: an upper bound on event payloads), `estimated_bytes` (`upload_bytes + event_bytes_max`) |
| `documents[]` | One per importable document (below) |
| `excluded[]` | `legacy_id`, `reason`, and `problems` (the scanner's blocking codes for that document) |
| `plugins[]` | `legacy_id`, `version`, `plugin_id`, `object` |
| `deferred[]` | Library files and plugins not uploaded: `kind`, `source`, optional `legacy_document`, `reason`, `bytes` |

Each `documents[]` entry has these fields:

| Field | Meaning |
|---|---|
| `legacy_id`, `status` | The folder name; `ready` or `degraded` |
| `problems` | The scanner's warning codes for the document, sorted and deduplicated |
| `warnings` | `name_replaced` (empty name, the legacy id is used), `name_truncated` (cut to 255 characters), `log_ignored` (the log doesn't continue the snapshot), `log_truncated` (damage or a sequence break stops replay early), `libraries_deferred` |
| `document` | The `documents` row: `id`, `name`, `state` (`active`), `current_seq` (the report's `last_accepted_seq`) |
| `external_mapping` | `system_id` `figcraft-server`, `resource_kind` `document`, `external_id` the legacy id, `internal_id` the document id |
| `snapshot` | The `document_snapshots` row `id` and `seq` (the snapshot's `session_seq`), and its `object` |
| `events` | `count` (`log.replayable` of a log that continues the snapshot, otherwise 0), `first_seq`/`last_seq` (`seq + 1` to `current_seq`, absent when there are none), `actor_principal_id`, `source_log_bytes` |
| `libraries[]` | Planned library objects |
| `links` | Links the import recreates from `links.json` |

A planned `object` has `id`, `kind` (`snapshot`, `library` or `plugin`), `bucket`, `source` (the
legacy file, relative to the scanned root), `key_prefix` and `key_suffix`, `size_bytes` and
`media_type`. The scanner hashes only plugin bundles, so only plugin objects carry `sha256` and
the full `object_key`. For snapshots and libraries the import computes the SHA-256 while
re-reading the file. The key is then `key_prefix` + hex digest + `key_suffix`, which matches
`object_metadata::keys`. A snapshot object holds the snapshot body: the file size minus the
17-byte `FCSN` header. Media types are `application/vnd.figcraft.snapshot`,
`application/vnd.figcraft.library` and `application/vnd.figcraft.plugin`.

### What is excluded or deferred

A document is excluded (`excluded[].reason`) when it is:

- `blocked`: marked blocked, or any blocking problem names it.
- `invalid_legacy_id`: not 1–64 ASCII letters, digits, `-` or `_`.
- `duplicate_legacy_id`: listed more than once. Every copy is excluded.
- `metadata_unavailable`: missing its metadata.
- `snapshot_unavailable`: without a decoded, checksum-valid snapshot that has a sequence and a
  non-empty body.
- `sequence_inconsistent`: `last_accepted_seq` isn't the snapshot sequence plus the replayable
  batches, or a sequence or size doesn't fit PostgreSQL `BIGINT`.

A library file or plugin is deferred (`deferred[].reason`) when it is:

- `library_unversioned`: `library.bin`, because the key layout needs a version.
- `library_name_invalid`: any other name, or a repeated version.
- `empty`.
- `plugin_bundle_invalid`: the bundle is missing or doesn't match the index.
- `plugin_version_invalid`: the plugin version cannot be a key segment: 1–64 lowercase letters,
  digits, `.` and `-`, starting with a letter or digit (for example `1.2.0` or `2.0.0-beta.1`).
- `plugin_id_invalid`, `plugin_digest_invalid`, `plugin_duplicate`, `size_out_of_range` or
  `limit_reached`.

## Import and cutover verification

`legacy_import::import_plan` (S3-018) executes a dry-run plan against the stopped server's data
directory. It refuses a plan with an unknown `format_version`, a `bucket` other than the store's,
or any blocker, except `exclusion_approval_required` when the operator passes
`approve_exclusions` after reviewing `excluded`. Excluded documents are never touched and are
returned as `skipped`. One failing document does not stop the others.

For each planned document, `import_document` first re-reads `snapshot.bin` and `log.bin`
read-only. It never follows links and caps reads by `ScanLimits`. The snapshot must still decode
at the planned sequence and size. The log's replayable accepted batches (same generation, after
the snapshot sequence, consecutive, up to the first damaged record) must still number
`events.count` and end at `document.current_seq`. Any difference is `legacy_changed`: rescan and
replan. Then four stages run. Each stage is idempotent and is detected from existing rows:

| Stage | Writes | Already done when |
| --- | --- | --- |
| `document` | `documents` row with the plan's id, name, workspace and project, `current_seq` = snapshot seq | the row exists (a different workspace, project or non-active state is `diverged`) |
| `mapping` | `external_mappings` (`figcraft-server`, `document`, legacy id) | the mapping exists for the same id (another id is `diverged`) |
| `snapshot` | content-addressed object (`keys::snapshot`, body = `SessionState` wire bytes), `objects` row registered and verified against the stored bytes, `document_snapshots` row registered and verified, a `document_snapshot` object reference, `current_snapshot_id` | object and snapshot rows are `verified` and the document points at the snapshot |
| `events` | one sequencer claim, then `append_events` in chunks of at most 256 batches or 8 MiB of legacy record bytes, one transaction each | `documents.current_seq` equals the plan's last sequence |

Events are appended only after the document's committed `current_seq`, so a rerun after a crash
in any stage continues where the last commit left off and never duplicates or skips a batch. A
`current_seq` below the snapshot or beyond the legacy log is `diverged`. A completed document
reruns as `already_complete` and writes nothing: no claim, no upload. Each outcome reports
whether this run created the document (`created`), added anything (`resumed`) or found
everything in place, plus per-stage flags and counts (`events_appended`,
`events_already_present`). `ImportOptions::stop_after` stops after a named stage, for staged
imports and crash rehearsal. Event rows carry `events.actor_principal_id` and the new claim's
fencing token. Replay of an imported document starts at the snapshot sequence; there are no
event rows at or below it.

#### Library files

After a document imports, `import_plan` imports each planned `files/<id>/library-<n>.bin` in
one transaction per file, and reports it in `libraries` (`created`, `resumed` or
`already_complete`, or an error). A library whose document failed is `document_not_imported`;
a run with `stop_after` before `libraries` skips them. The file is re-read bounded (at most
`MAX_LIBRARY_BYTES`, never a link) and must still have the planned size, then is validated as a
`.figcraft` archive (`library_rejected` otherwise). The legacy version number is kept:
`library-3.bin` becomes version 3 even when versions 1 and 2 never existed.
`libraries::publish_library_version` always takes `max + 1` and a random object id, so the import
has a narrow path of its own that writes:

- a `libraries` row with the plan's id (`derived_id(tenant, ["library", legacy id])`), active, no
  owning team, `source_document_id` = the imported document, named after the document;
- the object at the plan's id and key (`keys::library`), uploaded, registered and verified;
- a `library_versions` row (`derived_id(tenant, ["library_version", legacy id, n])`,
  `published_by` = the plan's principal) and its `library_version` object reference.

A rerun finds the version row and writes nothing. A version row at that number with another id,
object or digest, or a library row that is archived, team-owned or sourced from another
document, is `diverged`. A plan entry whose source, key prefix or object id does not re-derive
from the legacy id is `invalid_plan_entry`, so an edited plan can't read outside `files/<id>/`.

No `external_mappings` rows are written for libraries or plugins: the table's `resource_kind`
check admits only `tenant`, `workspace`, `project`, `document` and `principal`. Their ids are
derived from the legacy ids, so the plan itself is the mapping. Recording them needs a migration
that widens the check (deferred).

#### Plugin bundles

Plugins are imported on a full run (no `stop_after`) and reported in `plugins`. The legacy index
has no publisher and no manifest, so every imported plugin is published by
`figcraft-server-import` (`legacy_import::IMPORT_PUBLISHER`) under its legacy id as its name, and
`plugin_store::install_plugin_version` reads and checks the manifest from the bundle. The import
first creates the `plugins` row with the plan's id (`derived_id(tenant, ["plugin", legacy id])`);
the store then finds that row by name, so the bundle lands at the plan's `object_key`. The object row's id is the store's own, not the
plan's `object.id`. The bundle must still have the indexed size and SHA-256 (`legacy_changed`
otherwise). A bundle the store refuses (layout, manifest, a manifest `version` other than the
indexed one, a manifest `id` other than the plugin's other versions) is `plugin_rejected`. A live
plugin with that publisher and name under another id is `diverged`. A rerun finds the verified
version with the same digest and writes nothing.

#### Not imported

Share links are not imported. They need the share-token tables of S4-005; until then each
document's `links` stay in `not_imported` and block cutover.

### Cutover verification

`legacy_import::verify_import` (S3-019) is read-only. It takes a tenant transaction, which the
caller rolls back, and re-reads PostgreSQL, the object store and the legacy directory. For each
planned document it reports `passed` and a sorted list of fixed `failures` codes:

- `legacy_unreadable`, `legacy_changed`: the legacy files can't be re-read, or no longer match
  the plan.
- `document_missing`, `document_mismatch`: no row, or another workspace, project or a non-active
  state.
- `mapping_missing`, `mapping_mismatch`.
- `sequence_mismatch`: `current_seq` isn't the plan's last sequence.
- `snapshot_row_missing`, `snapshot_not_verified`, `current_snapshot_mismatch`.
- `object_missing`, `object_not_verified`: the row isn't verified or records another key, size
  or digest.
- `object_bytes_mismatch`: the stored bytes are missing or don't hash to the row.
- `snapshot_content_mismatch`: the stored bytes differ from the legacy snapshot body.
- `store_unavailable`.
- `event_count_mismatch`: the rows after the snapshot don't number `events.count` or don't span
  `first_seq..last_seq`.
- `replay_corrupt`: `read_events` found a bad checksum, bad decoding or a gap.
- `replay_mismatch`: the replayed batches differ from the log's replayable batches.

`libraries` and `plugins` hold one entry per planned library file and plugin bundle, each with
`passed` and sorted `failures`. They reuse `legacy_unreadable`, `legacy_changed`, `object_missing`,
`object_not_verified`, `object_bytes_mismatch` and `store_unavailable`, and add:

- `library_missing`, `library_mismatch`: no library row with the plan's id, or one that is not
  active, is team-owned or names another source document.
- `plugin_missing`, `plugin_mismatch`: no plugin row with the plan's id, or another publisher,
  name or a non-active state.
- `version_missing`, `version_mismatch`: no version row for the planned version, or one with
  another id, object or digest, or (plugins) not `verified`.
- `content_mismatch`: the stored bytes differ from the legacy file.

`not_imported` lists, by kind, what the import does not carry: each document's `links`, the
`library` files and `plugin` bundles that did not pass verification, `deferred` items and
`excluded_document`s. A verified library or plugin is not listed. `cutover_ready` is true only
when every document, library file and plugin passed and `not_imported` is empty.

Probe: `crates/cloud-storage/tests/legacy_import.rs` with `deploy/saas/legacy-import-preflight.sql`
and `legacy-import-cleanup.sql` (fixture tenant `ad000000-…-000000000001`).

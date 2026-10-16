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

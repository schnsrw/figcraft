# PostgreSQL client and migration decision

**Status:** Accepted for S2 · **Date:** 2026-10-16

FigCraft Cloud will use SQLx 0.9 from `figcraft-cloud-storage` as its asynchronous PostgreSQL
client. Migration scripts will be versioned SQL files embedded in the cloud binary and applied only
by an explicit migration command, not by normal service startup. SQLx's PostgreSQL migrator keeps
its default advisory lock enabled; S2-002 will define the command, lock behavior and failure tests.

The dependency disables defaults and enables only `macros`, `migrate`, `postgres`, `runtime-tokio`
and `tls-rustls-ring-webpki`. This avoids pulling in unrelated database drivers and uses Rustls for
TLS without requiring OpenSSL. SQLx 0.9 declares Rust 1.94 as its minimum toolchain and MIT OR
Apache-2.0 licensing, compatible with this workspace's Rust 1.95 MSRV and distribution policy.
The crate is confined to the cloud-storage adapter and is not part of the editor/WASM dependency
graph. The initial local adapter check used Rust 1.96, the oldest toolchain installed in the host
environment. S2-002's release-like Docker build then compiled the cloud image with the repository's
Rust 1.95 builder image, providing direct workspace-MSRV compatibility evidence. SQLx's declared
MSRV remains below the workspace floor. No database schema or runtime behavior is introduced by
S2-001.

Alternatives considered: `tokio-postgres` is a focused async PostgreSQL protocol client with the
same compatible license, but has no built-in migration system, so FigCraft would need to select and
integrate a separate migration framework. SQLx provides the required client and migration embedding
in one ecosystem, plus a PostgreSQL advisory-locking migrator. We will keep migrations as reviewed
plain SQL rather than introduce a schema DSL.

References:

- [SQLx 0.9 package metadata](https://docs.rs/crate/sqlx/0.9.0/source/Cargo.toml) — MSRV, license
  and feature names.
- [SQLx runtime and TLS support](https://docs.rs/sqlx/0.9.0/sqlx/) — Tokio and Rustls support.
- [SQLx embedded migration macro](https://docs.rs/sqlx/0.9.0/sqlx/macro.migrate.html) — binary
  embedding of versioned migration files.
- [SQLx migrator](https://docs.rs/sqlx/0.9.0/sqlx/migrate/struct.Migrator.html) — migration
  application, checksum validation and default advisory locking.
- [tokio-postgres crate](https://docs.rs/tokio-postgres/0.7.18/tokio_postgres/) — alternative
  async client.

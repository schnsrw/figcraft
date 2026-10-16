//! Object garbage collection with a dry run and minimum-age protection.
//!
//! [`collect_garbage`] runs one bounded pass over one tenant:
//!
//! 1. **Resume.** Objects left `deleting` by an earlier, interrupted pass are finished first, so a
//!    crash between any two steps below is repaired by simply running again.
//! 2. **Candidates.** [`collectable_objects`] lists unreferenced, unheld objects older than the
//!    requested minimum age, which is never lower than [`MIN_COLLECTION_AGE`]. Belt and braces, an
//!    object is also *protected* (left alone and reported) while anything still names it without a
//!    reference row: a document snapshot row or current-snapshot pointer, a library or plugin
//!    version, an export result, a thumbnail row, a `reserved`/`completed` upload reservation that
//!    would complete onto it, or a `queued`/`running` job whose input mentions its id or key. While
//!    any of the tenant's exports is `queued`/`running`, snapshot and asset objects are not
//!    collected at all: the export renders the document as of an earlier sequence and may need bytes
//!    whose references were dropped after it was queued.
//! 3. **Delete.** Per object, in its own short transaction, [`begin_object_deletion`] re-checks age,
//!    references and holds under the row lock and marks it `deleting` (setting `deleted_at`); the
//!    protections are re-checked in that same transaction. The bucket is versioned, so a plain
//!    `DELETE` would only add a delete marker: every version and delete marker of the exact key is
//!    listed (`ListObjectVersions`, bounded) and deleted by version id. Only once a listing comes
//!    back empty is the row marked `deleted` (`deleted_at` is kept). Any failure leaves the row
//!    `deleting` for the next pass; deleting a version twice is harmless.
//! 4. **Staging.** Overdue upload reservations are expired with [`expire_reservations`] and their
//!    staging keys removed, versions included. A key whose removal fails is reported and left to
//!    the staging lifecycle rule (S3-014), because the reservation no longer names it.
//!
//! A dry run performs only reads (each in a transaction that is rolled back) and reports what a
//! real pass would do: it marks, deletes and expires nothing.
//!
//! The pass is bounded: at most [`MAX_GC_BATCH`] objects, [`MAX_ACTIVE_JOBS_SCANNED`] active jobs
//! (more, or an active job input above [`MAX_JOB_INPUT_BYTES`], fails the pass closed), and per key
//! [`MAX_LISTING_ROUNDS`] listings of [`VERSION_PAGE`] entries. A candidate list is at most
//! [`MAX_COLLECTABLE_PAGE`] objects deep, so more than that many protected-but-unreferenced objects
//! (a missing reference row somewhere) would stall the pass behind them; they are all reported in
//! [`GcReport::protected`] for the reconciliation report (S3-016) to repair.
//!
//! Garbage collection runs as a `jobs` row of kind [`GC_JOB_KIND`] whose input
//! [`GcOptions::from_job_input`] parses. It acts for the system, never for a tenant member, and
//! never places or releases retention holds.

use std::fmt;
use std::time::Duration;

use figcraft_cloud_domain::{TenantId, Uuid};
use rusty_s3::{Method, S3Action};
use sqlx::PgPool;

use crate::object_references::{
    CollectableObject, DeletionStart, MAX_COLLECTABLE_PAGE, MAX_COLLECTION_AGE, MIN_COLLECTION_AGE, ObjectReferenceError, begin_object_deletion,
    collectable_objects,
};
use crate::objects::{ObjectStore, ObjectStoreError, REQUEST_TIMEOUT, valid_key};
use crate::uploads::{MAX_EXPIRE_BATCH, expire_reservations};
use crate::{TenantTransaction, begin_tenant_transaction};

/// The `jobs.kind` of a garbage-collection job.
pub const GC_JOB_KIND: &str = "object_garbage_collection";
/// The most objects (resumed plus new candidates) one pass handles.
pub const MAX_GC_BATCH: u32 = 1000;
/// The most `queued`/`running` jobs whose input a pass scans for object ids and keys.
pub const MAX_ACTIVE_JOBS_SCANNED: i64 = 10_000;
/// The largest active job input (as JSON text) a pass scans.
pub const MAX_JOB_INPUT_BYTES: i64 = 1024 * 1024;
/// Entries requested per `ListObjectVersions` page.
pub const VERSION_PAGE: u32 = 100;
/// The most listings per key, so one key costs at most `MAX_LISTING_ROUNDS * VERSION_PAGE` deletes.
pub const MAX_LISTING_ROUNDS: u32 = 100;
/// Largest `ListObjectVersions` response body read.
pub const MAX_LISTING_BYTES: usize = 256 * 1024;
/// Most XML nodes parsed from one listing.
const MAX_XML_NODES: u32 = 32_768;
/// Most entries accepted from one listing page (a store may ignore `max-keys`).
const MAX_PAGE_ENTRIES: usize = 1000;
/// Longest version id accepted from a listing.
const MAX_VERSION_ID_BYTES: usize = 1024;
/// How long a signature this module sends for itself stays valid.
const SELF_SIGNED_TTL: Duration = Duration::from_secs(60);

/// What one pass may do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GcOptions {
    /// Report only; change nothing in the database or the bucket.
    pub dry_run: bool,
    /// Most objects handled, 1..=[`MAX_GC_BATCH`]; also caps the reservations expired.
    pub limit: u32,
    /// Minimum object age; raised to [`MIN_COLLECTION_AGE`] and capped at [`MAX_COLLECTION_AGE`].
    pub min_age: Duration,
}

impl Default for GcOptions {
    /// A dry run of 100 objects at the minimum age: the safe thing to run by accident.
    fn default() -> Self {
        Self { dry_run: true, limit: 100, min_age: MIN_COLLECTION_AGE }
    }
}

impl GcOptions {
    /// Parse a [`GC_JOB_KIND`] job's input: `{"dry_run": bool, "limit": 1..=1000,
    /// "min_age_seconds": MIN..=MAX}` with `min_age_seconds` optional (default
    /// [`MIN_COLLECTION_AGE`]). Unknown fields, wrong types and out-of-range numbers are refused
    /// rather than clamped, so a misconfigured schedule fails visibly; in particular a minimum
    /// age below [`MIN_COLLECTION_AGE`] is `InvalidOptions`.
    pub fn from_job_input(input: &serde_json::Value) -> Result<Self, GcError> {
        let object = input.as_object().ok_or(GcError::InvalidOptions)?;
        if object.keys().any(|key| !matches!(key.as_str(), "dry_run" | "limit" | "min_age_seconds")) {
            return Err(GcError::InvalidOptions);
        }
        let dry_run = object.get("dry_run").and_then(serde_json::Value::as_bool).ok_or(GcError::InvalidOptions)?;
        let limit = object.get("limit").and_then(serde_json::Value::as_u64).ok_or(GcError::InvalidOptions)?;
        let limit = u32::try_from(limit).ok().filter(|limit| (1..=MAX_GC_BATCH).contains(limit)).ok_or(GcError::InvalidOptions)?;
        let min_age = match object.get("min_age_seconds") {
            None => MIN_COLLECTION_AGE,
            Some(value) => {
                let seconds = value.as_u64().ok_or(GcError::InvalidOptions)?;
                let age = Duration::from_secs(seconds);
                if age < MIN_COLLECTION_AGE || age > MAX_COLLECTION_AGE {
                    return Err(GcError::InvalidOptions);
                }
                age
            }
        };
        Ok(Self { dry_run, limit, min_age })
    }

    /// The job input [`from_job_input`](Self::from_job_input) reads back.
    #[must_use]
    pub fn to_job_input(&self) -> serde_json::Value {
        serde_json::json!({ "dry_run": self.dry_run, "limit": self.limit, "min_age_seconds": effective_min_age(self.min_age).as_secs() })
    }
}

fn effective_min_age(requested: Duration) -> Duration {
    Duration::from_secs(requested.clamp(MIN_COLLECTION_AGE, MAX_COLLECTION_AGE).as_secs())
}

/// Fixed, non-leaking categories for a pass that could not run or had to stop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GcError {
    /// A limit outside 1..=[`MAX_GC_BATCH`], or job input that does not parse.
    InvalidOptions,
    /// More than [`MAX_ACTIVE_JOBS_SCANNED`] active jobs, or one with input above
    /// [`MAX_JOB_INPUT_BYTES`]: the job check cannot be completed, so nothing is collected.
    ActiveJobsUnbounded,
    /// A tenant transaction could not be started.
    Transaction,
    Conflict,
    Database,
    CorruptRow,
}

impl fmt::Display for GcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "garbage collection error: {self:?}")
    }
}

impl std::error::Error for GcError {}

fn database_error(error: sqlx::Error) -> GcError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23505" | "40001" | "40P01") => GcError::Conflict,
        _ => GcError::Database,
    }
}

/// Why an unreferenced candidate was left alone.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Protection {
    /// A document's `current_snapshot_id` names a snapshot row stored in this object.
    CurrentSnapshot,
    /// A `document_snapshots` row is stored in this object.
    Snapshot,
    LibraryVersion,
    PluginVersion,
    /// An export's `result_object_id`.
    ExportResult,
    /// A queued or running export in the tenant pauses snapshot and asset collection.
    ActiveExport,
    Thumbnail,
    /// A `reserved` or `completed` upload reservation names this object, its key, or its kind and
    /// digest (completion would reuse it).
    UploadReservation,
    /// A queued or running job's input mentions the object's id or key.
    ActiveJob,
}

impl Protection {
    const ALL: [Self; 9] = [
        Self::CurrentSnapshot,
        Self::Snapshot,
        Self::LibraryVersion,
        Self::PluginVersion,
        Self::ExportResult,
        Self::ActiveExport,
        Self::Thumbnail,
        Self::UploadReservation,
        Self::ActiveJob,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CurrentSnapshot => "current_snapshot",
            Self::Snapshot => "snapshot",
            Self::LibraryVersion => "library_version",
            Self::PluginVersion => "plugin_version",
            Self::ExportResult => "export_result",
            Self::ActiveExport => "active_export",
            Self::Thumbnail => "thumbnail",
            Self::UploadReservation => "upload_reservation",
            Self::ActiveJob => "active_job",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|protection| protection.as_str() == value)
    }
}

/// Why a candidate was not marked for deletion (nothing changed for it).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GcSkip {
    /// A reference or hold appeared after the candidate list was read.
    Referenced,
    /// Younger than [`MIN_COLLECTION_AGE`] under the row lock.
    TooYoung,
    /// The row vanished or moved to a state that cannot be collected.
    Gone,
    /// A concurrent writer won; a later pass retries.
    Conflict,
    /// The row names a bucket other than the store's.
    ForeignBucket,
    /// The row's key is not one the store client will sign.
    UnsignableKey,
}

/// Why removing an object's bytes failed. The object stays `deleting` for the next pass.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GcFailure {
    Store(ObjectStoreError),
    /// A listing was too large or not the expected XML.
    MalformedListing,
    /// Versions remained after [`MAX_LISTING_ROUNDS`] listings.
    TooManyVersions,
    /// The row names a bucket other than the store's.
    ForeignBucket,
    /// The row's key is not one the store client will sign.
    UnsignableKey,
    /// A resumed `deleting` object is named again by a row listed in [`Protection`]; its bytes are
    /// left for an operator to look at.
    Protected(Protection),
}

/// What one pass did or, for a dry run, would do.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GcReport {
    pub dry_run: bool,
    /// The minimum age actually applied.
    pub min_age: Duration,
    /// Objects found `deleting` (left by an interrupted pass) that this pass finishes.
    pub resumed: Vec<CollectableObject>,
    /// Unprotected candidates this pass marks for deletion; each also ends in `deleted`,
    /// `skipped`, `protected` or `failed` on a real run.
    pub candidates: Vec<CollectableObject>,
    pub protected: Vec<(Uuid, Protection)>,
    pub skipped: Vec<(Uuid, GcSkip)>,
    /// Objects whose every version is gone and whose row is now `deleted`.
    pub deleted: Vec<Uuid>,
    pub failed: Vec<(Uuid, GcFailure)>,
    /// Object versions and delete markers removed (objects and staging keys).
    pub versions_removed: u64,
    /// Staging keys of reservations this pass expired (or would expire).
    pub staging_keys: Vec<String>,
    /// Staging keys whose removal failed; the staging lifecycle rule is their backstop.
    pub staging_failed: Vec<(String, GcFailure)>,
}

/// Run one garbage-collection pass over `tenant`'s objects in `store`'s bucket. See the module
/// documentation for the order of steps and what protects an object.
///
/// Returns `Err` only when the pass cannot start or a database step fails; per-object outcomes are
/// in the report. Every step is idempotent, so a pass that returned `Err` (or crashed) is
/// repaired by running again.
pub async fn collect_garbage(pool: &PgPool, store: &ObjectStore, tenant: TenantId, options: GcOptions) -> Result<GcReport, GcError> {
    if !(1..=MAX_GC_BATCH).contains(&options.limit) {
        return Err(GcError::InvalidOptions);
    }
    let min_age = effective_min_age(options.min_age);
    let mut report = GcReport { dry_run: options.dry_run, min_age, ..GcReport::default() };

    let mut scoped = begin(pool, tenant).await?;
    check_active_jobs_bounded(&mut scoped).await?;
    report.resumed = deleting_objects(&mut scoped, options.limit).await?;
    let remaining = options.limit.saturating_sub(u32::try_from(report.resumed.len()).unwrap_or(u32::MAX));
    if remaining > 0 {
        // Read a full page so protected objects at the head do not hide collectable ones behind them.
        let listed = collectable_objects(&mut scoped, min_age, MAX_COLLECTABLE_PAGE).await.map_err(reference_error)?;
        let ids: Vec<Uuid> = listed.iter().map(|object| object.id).collect();
        let protections = protections(&mut scoped, &ids).await?;
        for object in listed {
            if let Some((_, protection)) = protections.iter().find(|(id, _)| *id == object.id) {
                report.protected.push((object.id, *protection));
            } else if report.candidates.len() < usize::try_from(remaining).unwrap_or(usize::MAX) {
                report.candidates.push(object);
            }
        }
    }
    let staging_limit = i64::from(options.limit).min(MAX_EXPIRE_BATCH);
    if options.dry_run {
        report.staging_keys = due_staging_keys(&mut scoped, staging_limit).await?;
    }
    scoped.rollback().await.map_err(database_error)?;
    if options.dry_run {
        return Ok(report);
    }

    for object in report.resumed.clone() {
        resume(pool, store, tenant, &object, &mut report).await?;
    }
    for object in report.candidates.clone() {
        collect(pool, store, tenant, &object, &mut report).await?;
    }

    let mut scoped = begin(pool, tenant).await?;
    let keys = expire_reservations(&mut scoped, staging_limit).await.map_err(|_| GcError::Database)?;
    scoped.commit().await.map_err(database_error)?;
    for key in keys {
        match remove_all_versions(store, &key).await {
            Ok(removed) => report.versions_removed = report.versions_removed.saturating_add(removed),
            Err(failure) => report.staging_failed.push((key.clone(), failure)),
        }
        report.staging_keys.push(key);
    }
    Ok(report)
}

async fn begin(pool: &PgPool, tenant: TenantId) -> Result<TenantTransaction<'_>, GcError> {
    begin_tenant_transaction(pool, tenant).await.map_err(|_| GcError::Transaction)
}

fn reference_error(error: ObjectReferenceError) -> GcError {
    match error {
        ObjectReferenceError::InvalidLimit => GcError::InvalidOptions,
        ObjectReferenceError::Conflict => GcError::Conflict,
        ObjectReferenceError::CorruptRow => GcError::CorruptRow,
        _ => GcError::Database,
    }
}

/// Finish an object an earlier pass marked `deleting`.
async fn resume(pool: &PgPool, store: &ObjectStore, tenant: TenantId, object: &CollectableObject, report: &mut GcReport) -> Result<(), GcError> {
    if object.bucket != store.bucket_name() {
        report.failed.push((object.id, GcFailure::ForeignBucket));
        return Ok(());
    }
    let mut scoped = begin(pool, tenant).await?;
    let protection = protections(&mut scoped, &[object.id]).await?.first().map(|(_, protection)| *protection);
    scoped.rollback().await.map_err(database_error)?;
    if let Some(protection) = protection {
        report.failed.push((object.id, GcFailure::Protected(protection)));
        return Ok(());
    }
    remove_and_finish(pool, store, tenant, object, report).await
}

/// Mark one candidate `deleting` under its row lock, then remove its bytes.
async fn collect(pool: &PgPool, store: &ObjectStore, tenant: TenantId, object: &CollectableObject, report: &mut GcReport) -> Result<(), GcError> {
    if object.bucket != store.bucket_name() {
        report.skipped.push((object.id, GcSkip::ForeignBucket));
        return Ok(());
    }
    if !signable(store, &object.object_key) {
        report.skipped.push((object.id, GcSkip::UnsignableKey));
        return Ok(());
    }
    let mut scoped = begin(pool, tenant).await?;
    let marked = match begin_object_deletion(&mut scoped, object.id).await {
        Ok(DeletionStart::Marked(marked) | DeletionStart::AlreadyDeleting(marked)) => marked,
        Err(error) => {
            scoped.rollback().await.map_err(database_error)?;
            let skip = match error {
                ObjectReferenceError::Referenced => GcSkip::Referenced,
                ObjectReferenceError::TooYoung => GcSkip::TooYoung,
                ObjectReferenceError::NotFound | ObjectReferenceError::InvalidState => GcSkip::Gone,
                ObjectReferenceError::Conflict => GcSkip::Conflict,
                other => return Err(reference_error(other)),
            };
            report.skipped.push((object.id, skip));
            return Ok(());
        }
    };
    // Re-check the pointer tables in the transaction that holds the row lock.
    if let Some((_, protection)) = protections(&mut scoped, &[object.id]).await?.first() {
        scoped.rollback().await.map_err(database_error)?;
        report.protected.push((object.id, *protection));
        return Ok(());
    }
    scoped.commit().await.map_err(database_error)?;
    remove_and_finish(pool, store, tenant, &marked, report).await
}

/// Remove every version of a `deleting` object's key, then mark the row `deleted`.
async fn remove_and_finish(
    pool: &PgPool,
    store: &ObjectStore,
    tenant: TenantId,
    object: &CollectableObject,
    report: &mut GcReport,
) -> Result<(), GcError> {
    match remove_all_versions(store, &object.object_key).await {
        Ok(removed) => report.versions_removed = report.versions_removed.saturating_add(removed),
        Err(failure) => {
            report.failed.push((object.id, failure));
            return Ok(());
        }
    }
    let mut scoped = begin(pool, tenant).await?;
    sqlx::query("UPDATE objects SET state = 'deleted' WHERE tenant_id = $1::uuid AND id = $2::uuid AND state = 'deleting'")
        .bind(tenant.to_string())
        .bind(object.id.to_string())
        .execute(scoped.connection())
        .await
        .map_err(database_error)?;
    scoped.commit().await.map_err(database_error)?;
    // No row updated means a concurrent pass finished it first, which is the same outcome.
    report.deleted.push(object.id);
    Ok(())
}

/// Fail closed when the active-job scan in [`protections`] could not see every active job.
async fn check_active_jobs_bounded(transaction: &mut TenantTransaction<'_>) -> Result<(), GcError> {
    let (count, largest): (i64, i64) = sqlx::query_as(concat!(
        "SELECT count(*), coalesce(max(octet_length(j.input::text)), 0)::bigint FROM (SELECT input FROM jobs ",
        "WHERE tenant_id = $1::uuid AND state IN ('queued', 'running') LIMIT $2) j"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(MAX_ACTIVE_JOBS_SCANNED.saturating_add(1))
    .fetch_one(transaction.connection())
    .await
    .map_err(database_error)?;
    if count > MAX_ACTIVE_JOBS_SCANNED || largest > MAX_JOB_INPUT_BYTES { Err(GcError::ActiveJobsUnbounded) } else { Ok(()) }
}

/// Up to `limit` objects already `deleting`, longest-deleting first.
async fn deleting_objects(transaction: &mut TenantTransaction<'_>, limit: u32) -> Result<Vec<CollectableObject>, GcError> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(concat!(
        "SELECT id::text, bucket, object_key FROM objects WHERE tenant_id = $1::uuid AND state = 'deleting' ",
        "ORDER BY deleted_at, id LIMIT $2"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(i64::from(limit))
    .fetch_all(transaction.connection())
    .await
    .map_err(database_error)?;
    rows.into_iter()
        .map(|(id, bucket, object_key)| Ok(CollectableObject { id: id.parse().map_err(|_| GcError::CorruptRow)?, bucket, object_key }))
        .collect()
}

/// Staging keys of `reserved` reservations past their expiry, as [`expire_reservations`] would
/// pick them (without its row locks).
async fn due_staging_keys(transaction: &mut TenantTransaction<'_>, limit: i64) -> Result<Vec<String>, GcError> {
    sqlx::query_scalar(concat!(
        "SELECT staging_key FROM upload_reservations WHERE tenant_id = $1::uuid AND state = 'reserved' ",
        "AND expires_at <= clock_timestamp() ORDER BY expires_at, id LIMIT $2"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(limit.max(0))
    .fetch_all(transaction.connection())
    .await
    .map_err(database_error)
}

/// The first protection (in [`Protection::ALL`] order) of each of `ids` that has one. Ids of other
/// tenants' or unknown objects are ignored.
async fn protections(transaction: &mut TenantTransaction<'_>, ids: &[Uuid]) -> Result<Vec<(Uuid, Protection)>, GcError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<String> = ids.iter().map(Uuid::to_string).collect();
    let rows: Vec<(String, String)> = sqlx::query_as(concat!(
        "WITH c AS (SELECT o.id, o.kind, o.sha256, o.object_key FROM objects o ",
        "WHERE o.tenant_id = $1::uuid AND o.id = ANY ($2::text[]::uuid[])), ",
        "active_jobs AS (SELECT j.input::text AS input FROM jobs j WHERE j.tenant_id = $1::uuid ",
        "AND j.state IN ('queued', 'running') ORDER BY j.created_at, j.id LIMIT $3), ",
        "verdict AS (SELECT c.id, CASE ",
        "WHEN EXISTS (SELECT 1 FROM documents d JOIN document_snapshots s ON s.tenant_id = d.tenant_id ",
        "AND s.document_id = d.id AND s.id = d.current_snapshot_id WHERE d.tenant_id = $1::uuid AND s.object_id = c.id) ",
        "THEN 'current_snapshot' ",
        "WHEN EXISTS (SELECT 1 FROM document_snapshots s WHERE s.tenant_id = $1::uuid AND s.object_id = c.id) THEN 'snapshot' ",
        "WHEN EXISTS (SELECT 1 FROM library_versions v WHERE v.tenant_id = $1::uuid AND v.object_id = c.id) THEN 'library_version' ",
        "WHEN EXISTS (SELECT 1 FROM plugin_versions v WHERE v.tenant_id = $1::uuid AND v.object_id = c.id) THEN 'plugin_version' ",
        "WHEN EXISTS (SELECT 1 FROM exports e WHERE e.tenant_id = $1::uuid AND e.result_object_id = c.id) THEN 'export_result' ",
        "WHEN c.kind IN ('snapshot', 'asset') AND EXISTS (SELECT 1 FROM exports e WHERE e.tenant_id = $1::uuid ",
        "AND e.state IN ('queued', 'running')) THEN 'active_export' ",
        "WHEN EXISTS (SELECT 1 FROM document_thumbnails t WHERE t.tenant_id = $1::uuid AND t.object_id = c.id) THEN 'thumbnail' ",
        "WHEN EXISTS (SELECT 1 FROM upload_reservations r WHERE r.tenant_id = $1::uuid AND r.state IN ('reserved', 'completed') ",
        "AND (r.object_id = c.id OR r.staging_key = c.object_key OR (r.kind = c.kind AND r.expected_sha256 = c.sha256))) ",
        "THEN 'upload_reservation' ",
        "WHEN EXISTS (SELECT 1 FROM active_jobs j WHERE strpos(j.input, c.id::text) > 0 OR strpos(j.input, c.object_key) > 0) ",
        "THEN 'active_job' ",
        "END AS protection FROM c) ",
        "SELECT id::text, protection FROM verdict WHERE protection IS NOT NULL"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(ids)
    .bind(MAX_ACTIVE_JOBS_SCANNED)
    .fetch_all(transaction.connection())
    .await
    .map_err(database_error)?;
    rows.iter()
        .map(|(id, protection)| {
            let id = id.parse().map_err(|_| GcError::CorruptRow)?;
            Protection::parse(protection).map(|protection| (id, protection)).ok_or(GcError::CorruptRow)
        })
        .collect()
}

/// Whether the store client can sign requests for `key`. `rusty-s3` unwraps object URLs
/// internally, so a key must pass this before any object action is built from it.
fn signable(store: &ObjectStore, key: &str) -> bool {
    valid_key(key) && store.bucket.object_url(key).is_ok()
}

/// Delete every version and delete marker of exactly `key` and return how many were removed.
/// Stores without `ListObjectVersions` get one plain `DELETE` instead.
///
/// Listing is sorted by key and `key` is the smallest key with prefix `key`, so a page without
/// an entry for `key` proves none remain, whatever longer keys share the prefix.
async fn remove_all_versions(store: &ObjectStore, key: &str) -> Result<u64, GcFailure> {
    if !signable(store, key) {
        return Err(GcFailure::UnsignableKey);
    }
    let mut removed: u64 = 0;
    for _ in 0..MAX_LISTING_ROUNDS {
        let Some(versions) = list_versions(store, key).await? else {
            store.delete_object(key).await.map_err(GcFailure::Store)?;
            return Ok(removed);
        };
        if versions.is_empty() {
            return Ok(removed);
        }
        for version in &versions {
            delete_version(store, key, version).await?;
            removed = removed.saturating_add(1);
        }
    }
    Err(GcFailure::TooManyVersions)
}

/// Version ids of `key` on one listing page, or `None` when the store does not implement
/// `ListObjectVersions`. `key` must be [`signable`].
async fn list_versions(store: &ObjectStore, key: &str) -> Result<Option<Vec<String>>, GcFailure> {
    let page = VERSION_PAGE.to_string();
    // `sign` merges these with its own parameters and requires them sorted by name.
    let query = [("max-keys", page.as_str()), ("prefix", key), ("versions", "")];
    // The base URL was validated as an `http(s)` URL with a host when the store was built, which
    // is everything `sign` requires.
    let url = rusty_s3::signing::sign(
        &jiff::Timestamp::now(),
        Method::Get,
        store.bucket.base_url().clone(),
        store.credentials.key(),
        store.credentials.secret(),
        store.credentials.token(),
        store.bucket.region(),
        SELF_SIGNED_TTL.as_secs(),
        query.into_iter(),
        std::iter::empty(),
    );
    let mut response =
        store.client.get(url.as_str()).timeout(REQUEST_TIMEOUT).send().await.map_err(|_| GcFailure::Store(ObjectStoreError::Unreachable))?;
    let status = response.status().as_u16();
    if response.content_length().is_some_and(|length| length > MAX_LISTING_BYTES as u64) {
        return Err(GcFailure::MalformedListing);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| GcFailure::Store(ObjectStoreError::Unreachable))? {
        if body.len().saturating_add(chunk.len()) > MAX_LISTING_BYTES {
            return Err(GcFailure::MalformedListing);
        }
        body.extend_from_slice(&chunk);
    }
    match status {
        200 => parse_versions(&body, key).map(Some),
        501 => Ok(None),
        _ if error_code(&body).as_deref() == Some("NotImplemented") => Ok(None),
        401 | 403 => Err(GcFailure::Store(ObjectStoreError::Unauthorized)),
        404 => Err(GcFailure::Store(ObjectStoreError::BucketMissing)),
        _ => Err(GcFailure::Store(ObjectStoreError::Unexpected)),
    }
}

/// Delete one version (or delete marker) of `key`. A version already gone succeeds.
async fn delete_version(store: &ObjectStore, key: &str, version: &str) -> Result<(), GcFailure> {
    if !signable(store, key) || !valid_version_id(version) {
        return Err(GcFailure::UnsignableKey);
    }
    let mut action = store.bucket.delete_object(Some(&store.credentials), key);
    action.query_mut().insert("versionId", version.to_string());
    let url = action.sign(SELF_SIGNED_TTL);
    let response =
        store.client.delete(url.as_str()).timeout(REQUEST_TIMEOUT).send().await.map_err(|_| GcFailure::Store(ObjectStoreError::Unreachable))?;
    match response.status().as_u16() {
        200..=299 | 404 => Ok(()),
        401 | 403 => Err(GcFailure::Store(ObjectStoreError::Unauthorized)),
        _ => Err(GcFailure::Store(ObjectStoreError::Unexpected)),
    }
}

/// A version id from a listing: 1–1024 printable ASCII bytes (`null` included, for objects
/// written before versioning was enabled).
fn valid_version_id(version: &str) -> bool {
    (1..=MAX_VERSION_ID_BYTES).contains(&version.len()) && version.bytes().all(|byte| byte.is_ascii_graphic())
}

fn parse_document(body: &[u8]) -> Option<roxmltree::Document<'_>> {
    if body.len() > MAX_LISTING_BYTES {
        return None;
    }
    let text = std::str::from_utf8(body).ok()?;
    let options = roxmltree::ParsingOptions { allow_dtd: false, nodes_limit: MAX_XML_NODES, ..roxmltree::ParsingOptions::default() };
    roxmltree::Document::parse_with_options(text, options).ok()
}

fn child<'a, 'input>(node: roxmltree::Node<'a, 'input>, name: &str) -> Option<roxmltree::Node<'a, 'input>> {
    node.children().find(|child| child.is_element() && child.tag_name().name() == name)
}

fn text<'a>(node: roxmltree::Node<'a, '_>) -> &'a str {
    node.text().unwrap_or("").trim()
}

/// The version ids of exactly `key` in a `ListVersionsResult`, versions and delete markers alike.
fn parse_versions(body: &[u8], key: &str) -> Result<Vec<String>, GcFailure> {
    let document = parse_document(body).ok_or(GcFailure::MalformedListing)?;
    let root = document.root_element();
    if root.tag_name().name() != "ListVersionsResult" {
        return Err(GcFailure::MalformedListing);
    }
    let mut versions = Vec::new();
    let mut entries = 0usize;
    for entry in root.children().filter(|node| node.is_element() && matches!(node.tag_name().name(), "Version" | "DeleteMarker")) {
        entries = entries.saturating_add(1);
        if entries > MAX_PAGE_ENTRIES {
            return Err(GcFailure::MalformedListing);
        }
        let entry_key = child(entry, "Key").map(text).ok_or(GcFailure::MalformedListing)?;
        if entry_key != key {
            continue;
        }
        let version = child(entry, "VersionId").map(text).ok_or(GcFailure::MalformedListing)?;
        if !valid_version_id(version) {
            return Err(GcFailure::MalformedListing);
        }
        if !versions.iter().any(|known: &String| known == version) {
            versions.push(version.to_string());
        }
    }
    Ok(versions)
}

/// The S3 error `<Code>` of an error document: 1–64 ASCII letters and digits, or nothing.
fn error_code(body: &[u8]) -> Option<String> {
    let document = parse_document(body)?;
    let root = document.root_element();
    if root.tag_name().name() != "Error" {
        return None;
    }
    let code = text(child(root, "Code")?);
    let valid = (1..=64).contains(&code.len()) && code.bytes().all(|byte| byte.is_ascii_alphanumeric());
    valid.then(|| code.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "tenants/ab000000-0000-4000-8000-000000000001/assets/ab";

    fn listing(entries: &str) -> String {
        format!(
            r#"<?xml version="1.0"?><ListVersionsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><IsTruncated>false</IsTruncated>{entries}</ListVersionsResult>"#
        )
    }

    #[test]
    fn listings_yield_versions_and_delete_markers_of_exactly_the_key() {
        let body = listing(&format!(
            concat!(
                "<Version><Key>{key}</Key><VersionId>v1</VersionId><IsLatest>false</IsLatest></Version>",
                "<DeleteMarker><Key>{key}</Key><VersionId>m1</VersionId><IsLatest>true</IsLatest></DeleteMarker>",
                "<Version><Key>{key}cd</Key><VersionId>other</VersionId></Version>",
                "<Version><Key>{key}</Key><VersionId>null</VersionId></Version>",
                "<Version><Key>{key}</Key><VersionId>v1</VersionId></Version>",
                "<CommonPrefixes><Prefix>x</Prefix></CommonPrefixes>"
            ),
            key = KEY
        ));
        assert_eq!(parse_versions(body.as_bytes(), KEY), Ok(vec!["v1".to_string(), "m1".to_string(), "null".to_string()]));
        assert_eq!(parse_versions(listing("").as_bytes(), KEY), Ok(Vec::new()));
    }

    #[test]
    fn malformed_listings_are_refused() {
        for body in [
            String::new(),
            "<ListBucketResult></ListBucketResult>".into(),
            listing("<Version><VersionId>v</VersionId></Version>"),
            listing(&format!("<Version><Key>{KEY}</Key></Version>")),
            listing(&format!("<Version><Key>{KEY}</Key><VersionId></VersionId></Version>")),
            listing(&format!("<Version><Key>{KEY}</Key><VersionId>a b</VersionId></Version>")),
            listing(&format!("<Version><Key>{KEY}</Key><VersionId>{}</VersionId></Version>", "v".repeat(MAX_VERSION_ID_BYTES + 1))),
            listing(&"<Version><Key>k</Key><VersionId>v</VersionId></Version>".repeat(MAX_PAGE_ENTRIES + 1)),
            format!("<!DOCTYPE x [<!ENTITY a \"b\">]>{}", listing("")),
            "x".repeat(MAX_LISTING_BYTES + 1),
        ] {
            assert_eq!(parse_versions(body.as_bytes(), KEY), Err(GcFailure::MalformedListing), "{}", &body[..body.len().min(120)]);
        }
    }

    #[test]
    fn junk_listings_never_panic() {
        let seed = listing(&format!("<Version><Key>{KEY}</Key><VersionId>v1</VersionId></Version><IsTruncated>true</IsTruncated>"));
        let bytes = seed.as_bytes();
        // Every truncation, every single-byte replacement from a small alphabet, and seeded noise.
        for end in 0..bytes.len() {
            let _ = parse_versions(&bytes[..end], KEY);
            let _ = error_code(&bytes[..end]);
        }
        for index in 0..bytes.len() {
            for replacement in [b'<', b'>', b'/', b'&', b'"', 0, 0xff, b'x'] {
                let mut mutated = bytes.to_vec();
                if let Some(byte) = mutated.get_mut(index) {
                    *byte = replacement;
                }
                let _ = parse_versions(&mutated, KEY);
                let _ = error_code(&mutated);
            }
        }
        const ALPHABET: &[u8] = b"<>/ListVersionsResultVersionKeyId &;\"=x";
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        for length in 0..2000usize {
            let noise: Vec<u8> = (0..length % 300)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    ALPHABET[usize::try_from(state % ALPHABET.len() as u64).unwrap_or(0)]
                })
                .collect();
            let _ = parse_versions(&noise, KEY);
            let _ = error_code(&noise);
        }
    }

    #[test]
    fn error_codes_are_bounded_alphanumerics() {
        assert_eq!(error_code(b"<Error><Code>NotImplemented</Code></Error>").as_deref(), Some("NotImplemented"));
        for body in ["<Error><Code></Code></Error>", "<Error><Code>a b</Code></Error>", "<Other><Code>X</Code></Other>", "", "<Error>"] {
            assert_eq!(error_code(body.as_bytes()), None, "{body}");
        }
        assert_eq!(error_code(format!("<Error><Code>{}</Code></Error>", "A".repeat(65)).as_bytes()), None);
    }

    #[test]
    fn protections_round_trip_and_junk_does_not_parse() {
        for protection in Protection::ALL {
            assert_eq!(Protection::parse(protection.as_str()), Some(protection));
        }
        for junk in ["", " ", "Snapshot", "snapshot ", "retention_hold", "\0", "😀", &"x".repeat(4096)] {
            assert_eq!(Protection::parse(junk), None);
        }
    }

    #[test]
    fn job_input_round_trips_and_refuses_lower_ages_and_junk() {
        let options = GcOptions { dry_run: false, limit: 250, min_age: MIN_COLLECTION_AGE * 3 };
        assert_eq!(GcOptions::from_job_input(&options.to_job_input()), Ok(options));
        assert_eq!(
            GcOptions::from_job_input(&serde_json::json!({ "dry_run": true, "limit": 1 })),
            Ok(GcOptions { dry_run: true, limit: 1, min_age: MIN_COLLECTION_AGE })
        );
        // A lower requested age is raised when written, so the round trip stays valid.
        let lower = GcOptions { min_age: Duration::ZERO, ..options };
        assert_eq!(GcOptions::from_job_input(&lower.to_job_input()).map(|parsed| parsed.min_age), Ok(MIN_COLLECTION_AGE));

        let floor = MIN_COLLECTION_AGE.as_secs();
        let cap = MAX_COLLECTION_AGE.as_secs();
        let mut junk = vec![
            serde_json::json!(null),
            serde_json::json!([]),
            serde_json::json!("dry_run"),
            serde_json::json!(1),
            serde_json::json!({}),
            serde_json::json!({ "dry_run": true }),
            serde_json::json!({ "limit": 1 }),
            serde_json::json!({ "dry_run": "true", "limit": 1 }),
            serde_json::json!({ "dry_run": 1, "limit": 1 }),
            serde_json::json!({ "dry_run": true, "limit": 0 }),
            serde_json::json!({ "dry_run": true, "limit": -1 }),
            serde_json::json!({ "dry_run": true, "limit": 1.5 }),
            serde_json::json!({ "dry_run": true, "limit": MAX_GC_BATCH + 1 }),
            serde_json::json!({ "dry_run": true, "limit": u64::MAX }),
            serde_json::json!({ "dry_run": true, "limit": "1" }),
            serde_json::json!({ "dry_run": true, "limit": 1, "min_age_seconds": 0 }),
            serde_json::json!({ "dry_run": true, "limit": 1, "min_age_seconds": floor - 1 }),
            serde_json::json!({ "dry_run": true, "limit": 1, "min_age_seconds": cap + 1 }),
            serde_json::json!({ "dry_run": true, "limit": 1, "min_age_seconds": -86_400 }),
            serde_json::json!({ "dry_run": true, "limit": 1, "min_age_seconds": null }),
            serde_json::json!({ "dry_run": true, "limit": 1, "min_age_seconds": 1e300 }),
            serde_json::json!({ "dry_run": true, "limit": 1, "force": true }),
            serde_json::json!({ "dry_run": true, "limit": 1, "": 1 }),
        ];
        for key in ["DRY_RUN", "dry_run ", "limit\0", "min_age", "😀"] {
            junk.push(serde_json::json!({ "dry_run": true, "limit": 1, key: 1 }));
        }
        for input in &junk {
            assert_eq!(GcOptions::from_job_input(input), Err(GcError::InvalidOptions), "{input}");
        }
        assert_eq!(
            GcOptions::from_job_input(&serde_json::json!({ "dry_run": true, "limit": 1, "min_age_seconds": floor })).map(|o| o.min_age),
            Ok(MIN_COLLECTION_AGE)
        );
        assert_eq!(
            GcOptions::from_job_input(&serde_json::json!({ "dry_run": true, "limit": 1, "min_age_seconds": cap })).map(|o| o.min_age),
            Ok(MAX_COLLECTION_AGE)
        );
    }

    #[test]
    fn the_minimum_age_is_never_lowered() {
        for requested in [Duration::ZERO, Duration::from_secs(1), MIN_COLLECTION_AGE - Duration::from_nanos(1)] {
            assert_eq!(effective_min_age(requested), MIN_COLLECTION_AGE);
        }
        assert_eq!(effective_min_age(MIN_COLLECTION_AGE * 2), MIN_COLLECTION_AGE * 2);
        assert_eq!(effective_min_age(Duration::MAX), MAX_COLLECTION_AGE);
        assert!(GcOptions::default().dry_run);
    }

    #[test]
    fn version_ids_are_bounded_printable_ascii() {
        for version in ["null", "3sL4kqtJlcpXroDTDmJ+rmSpXd3dIbrHY+MTRCxf3vjVBH40Nr8X8gdRQBpUMLUo", "a"] {
            assert!(valid_version_id(version));
        }
        for version in ["", " ", "a b", "a\nb", "é", &"v".repeat(MAX_VERSION_ID_BYTES + 1)] {
            assert!(!valid_version_id(version), "{version:?}");
        }
    }

    #[tokio::test]
    async fn bad_limits_are_refused_before_any_io() {
        let store = ObjectStore::new(&crate::objects::ObjectStoreSettings {
            endpoint: "http://127.0.0.1:9".into(),
            region: "us-east-1".into(),
            bucket: "figcraft".into(),
            access_key: "FCaccess".into(),
            secret_key: "secret-value-123".into(),
            path_style: true,
        })
        .unwrap();
        // A lazy pool never connects unless a query runs.
        let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://nobody@127.0.0.1:9/none").unwrap();
        let tenant: TenantId = "ab000000-0000-4000-8000-000000000001".parse().unwrap();
        for limit in [0, MAX_GC_BATCH + 1, u32::MAX] {
            let options = GcOptions { dry_run: true, limit, min_age: MIN_COLLECTION_AGE };
            assert_eq!(collect_garbage(&pool, &store, tenant, options).await, Err(GcError::InvalidOptions));
        }
        assert_eq!(remove_all_versions(&store, "a/../b").await, Err(GcFailure::UnsignableKey));
        assert_eq!(remove_all_versions(&store, "a/b").await, Err(GcFailure::Store(ObjectStoreError::Unreachable)));
        assert_eq!(delete_version(&store, "a/b", "a b").await, Err(GcFailure::UnsignableKey));
    }
}

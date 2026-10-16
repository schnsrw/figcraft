//! Database-to-bucket reconciliation report (S3-016).
//!
//! [`reconcile_tenant`] lists the bucket under `tenants/<tenant>/` with `ListObjectsV2` and
//! compares every listed key with the tenant's `objects` rows (and, for staging keys, with
//! `upload_reservations`). It reports:
//!
//! - **missing objects**: a row whose key is not in the bucket. A `verified` row is critical (its
//!   bytes are lost or were deleted behind the database's back); a `pending` row is informational.
//!   `rejected` and `deleting` rows may legitimately have no bytes and are not reported.
//! - **orphan keys**: a listed key with no live row (no row at all, or only `deleted` rows).
//! - **stale staging keys**: a staging key whose reservation is missing, expired or already closed
//!   (promotion and rejection delete staged bytes, and the expiry sweep should too).
//! - **size mismatches**: the listed size differs from `size_bytes` (critical when `verified`).
//! - **stuck objects**: `pending` longer than [`ReconciliationLimits::stuck_after`] since creation,
//!   or `deleting` that long since deletion started.
//! - **collectable objects**: `pending`, `verified` or `rejected` rows with no reference and no
//!   retention hold (informational; [`MIN_COLLECTION_AGE`] decides eligibility).
//!
//! plus counts and bytes per kind. The format is described in `docs/saas/deployment-operations.md`
//! ("Object reconciliation report").
//!
//! # Read-only
//!
//! The function sends only signed `GET` listing requests and runs only `SELECT` statements, and it
//! first switches the caller's transaction to `READ ONLY` (allowed at any point in a PostgreSQL
//! transaction), so even a future mistake cannot write: the database refuses it. Run it in a
//! transaction of its own and roll it back or commit it afterwards.
//!
//! # Consistency
//!
//! The listing and the row scan are not one atomic snapshot. The database clock is read before the
//! listing starts; rows created or verified after that instant are counted but never reported
//! missing or mismatched (their bytes may have been written after the listing passed their key).
//! Rows are read after the listing, so a key written and registered while the listing ran is
//! matched rather than reported orphaned. An orphan whose row is registered only after the row
//! scan (a write that has put its bytes but not yet committed its row) can still appear; re-run the
//! report before acting on a young orphan (`last_modified` says how young).
//!
//! # Bounds
//!
//! Rows, keys, listing pages and findings per category are capped by [`ReconciliationLimits`]
//! (hard ceilings below). Reaching any cap marks the report incomplete and names the cap. When the
//! listing or the row scan stops early, comparisons are limited to the key range both sides fully
//! covered, so a cap never produces a false "missing" or "orphan" finding. Listing responses are
//! read up to [`MAX_LIST_RESPONSE_BYTES`] and parsed with `roxmltree` under a node limit, without
//! DTDs; every listed key is percent-decoded, bounded and checked against the tenant prefix.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::time::Duration;

use figcraft_cloud_domain::{TenantId, Uuid};
use serde::{Deserialize, Serialize};

use crate::TenantTransaction;
use crate::object_metadata::ObjectKind;
use crate::object_references::MIN_COLLECTION_AGE;
use crate::objects::{MAX_KEY_BYTES, ObjectStore, ObjectStoreError, REQUEST_TIMEOUT, valid_key};

/// Version of the [`ReconciliationReport`] layout. Bumped on any change a consumer must notice;
/// adding an optional field or a new reason code is not one.
pub const REPORT_FORMAT_VERSION: u32 = 1;
/// Hard ceiling on [`ReconciliationLimits::max_rows`].
pub const MAX_ROWS: u32 = 1_000_000;
/// Hard ceiling on [`ReconciliationLimits::max_keys`].
pub const MAX_KEYS: u32 = 1_000_000;
/// Hard ceiling on [`ReconciliationLimits::max_pages`].
pub const MAX_PAGES: u32 = 10_000;
/// Hard ceiling on [`ReconciliationLimits::page_size`] (the S3 maximum).
pub const MAX_PAGE_SIZE: u32 = 1000;
/// Hard ceiling on [`ReconciliationLimits::max_findings`].
pub const MAX_FINDINGS: u32 = 10_000;
/// Longest [`ReconciliationLimits::stuck_after`] (ten years).
pub const MAX_STUCK_AFTER: Duration = Duration::from_secs(10 * 366 * 24 * 60 * 60);
/// Largest listing response body read.
pub const MAX_LIST_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// Most XML nodes parsed from one listing response (about 30 per listed key).
const MAX_LIST_XML_NODES: u32 = 64 * 1024;
/// Longest continuation token followed.
const MAX_TOKEN_BYTES: usize = 1024;
/// Longest `LastModified` value carried into the report.
const MAX_TIMESTAMP_BYTES: usize = 64;
/// Rows fetched per database round trip.
const ROW_PAGE: u32 = 1000;
/// Staging keys looked up per reservation query.
const STAGING_LOOKUP_CHUNK: usize = 1000;
/// How long a listing request's signature stays valid.
const SELF_SIGNED_TTL: Duration = Duration::from_secs(60);

const KINDS: [ObjectKind; 6] =
    [ObjectKind::Snapshot, ObjectKind::Asset, ObjectKind::Library, ObjectKind::Plugin, ObjectKind::Export, ObjectKind::Thumbnail];

/// Fixed, non-leaking categories for reconciliation failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReconciliationError {
    /// A limit is zero or above its ceiling.
    InvalidLimits,
    /// The transaction could not be switched to read-only.
    ReadOnly,
    /// The listing request failed (`Unreachable`, `Unauthorized`, `BucketMissing`, `Unexpected`).
    Store(ObjectStoreError),
    /// A listing response was too large, not the expected XML, or listed a key outside the prefix.
    MalformedListing,
    Database,
    CorruptRow,
}

impl fmt::Display for ReconciliationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "reconciliation error: {self:?}")
    }
}

impl std::error::Error for ReconciliationError {}

fn database_error(_: sqlx::Error) -> ReconciliationError {
    ReconciliationError::Database
}

/// Bounds on one report.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReconciliationLimits {
    /// Most `objects` rows read (1..=[`MAX_ROWS`]).
    pub max_rows: u32,
    /// Most bucket keys listed (1..=[`MAX_KEYS`]).
    pub max_keys: u32,
    /// Most `ListObjectsV2` pages requested (1..=[`MAX_PAGES`]).
    pub max_pages: u32,
    /// Keys requested per page (1..=[`MAX_PAGE_SIZE`]).
    pub page_size: u32,
    /// Most findings listed per category (1..=[`MAX_FINDINGS`]); counts stay exact past it.
    pub max_findings: u32,
    /// How long a row may stay `pending` (since creation) or `deleting` (since deletion started)
    /// before it is reported stuck (1 second..=[`MAX_STUCK_AFTER`]).
    pub stuck_after: Duration,
}

impl Default for ReconciliationLimits {
    fn default() -> Self {
        Self { max_rows: 100_000, max_keys: 100_000, max_pages: 200, page_size: 1000, max_findings: 1000, stuck_after: Duration::from_secs(60 * 60) }
    }
}

impl ReconciliationLimits {
    fn validate(&self) -> Result<(), ReconciliationError> {
        let valid = (1..=MAX_ROWS).contains(&self.max_rows)
            && (1..=MAX_KEYS).contains(&self.max_keys)
            && (1..=MAX_PAGES).contains(&self.max_pages)
            && (1..=MAX_PAGE_SIZE).contains(&self.page_size)
            && (1..=MAX_FINDINGS).contains(&self.max_findings)
            && self.stuck_after >= Duration::from_secs(1)
            && self.stuck_after <= MAX_STUCK_AFTER;
        if valid { Ok(()) } else { Err(ReconciliationError::InvalidLimits) }
    }
}

/// How urgently a finding needs an operator.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Data the database promises is gone or wrong.
    Critical,
    /// Storage the database does not account for, or work that did not finish.
    Warning,
    /// Expected states worth knowing about.
    Info,
}

/// A cap that stopped the report early.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cap {
    Rows,
    Keys,
    Pages,
    Findings,
}

/// An `objects.state` value as reported.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowState {
    Pending,
    Verified,
    Rejected,
    Deleting,
    Deleted,
}

impl RowState {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "verified" => Some(Self::Verified),
            "rejected" => Some(Self::Rejected),
            "deleting" => Some(Self::Deleting),
            "deleted" => Some(Self::Deleted),
            _ => None,
        }
    }
}

/// Why a listed key is an orphan.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrphanReason {
    /// No `objects` row names the key.
    NoRow,
    /// Only `deleted` rows name the key: the garbage collector marked it deleted but the bytes stayed.
    DeletedRowOnly,
    /// The key is not one FigCraft writes or signs (`objects::valid_key`), so it was put by
    /// something else.
    UnsignableKey,
}

/// Why a staging key is stale.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StagingReason {
    /// No reservation has this staging key.
    NoReservation,
    /// The reservation expired (or is `reserved` past its expiry) and the sweep has not removed the bytes.
    ReservationExpired,
    /// The reservation was promoted, rejected or completed, which should have removed the bytes.
    ReservationClosed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MissingObject {
    pub severity: Severity,
    pub object_id: Uuid,
    pub kind: String,
    pub state: RowState,
    pub object_key: String,
    pub size_bytes: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OrphanKey {
    pub severity: Severity,
    pub reason: OrphanReason,
    pub object_key: String,
    pub stored_bytes: u64,
    /// The store's `LastModified`, verbatim (ISO 8601), or empty when absent or malformed.
    pub last_modified: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StaleStagingKey {
    pub severity: Severity,
    pub reason: StagingReason,
    pub object_key: String,
    pub reservation_id: Option<Uuid>,
    pub stored_bytes: u64,
    pub last_modified: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SizeMismatch {
    pub severity: Severity,
    pub object_id: Uuid,
    pub kind: String,
    pub state: RowState,
    pub object_key: String,
    pub recorded_bytes: i64,
    pub stored_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StuckObject {
    pub severity: Severity,
    pub object_id: Uuid,
    pub kind: String,
    pub state: RowState,
    pub object_key: String,
    /// Seconds since creation (`pending`) or since deletion started (`deleting`).
    pub age_seconds: i64,
    /// Whether the key was listed; `None` when it lies beyond what the listing covered.
    pub in_bucket: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CollectableObject {
    pub severity: Severity,
    pub object_id: Uuid,
    pub kind: String,
    pub state: RowState,
    pub object_key: String,
    pub size_bytes: i64,
    pub age_seconds: i64,
    /// Old enough for `object_references::collectable_objects` ([`MIN_COLLECTION_AGE`]).
    pub eligible: bool,
}

/// Exact finding counts per category, whatever the finding cap.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct FindingCounts {
    pub missing_objects: u64,
    pub orphan_keys: u64,
    pub stale_staging_keys: u64,
    pub size_mismatches: u64,
    pub stuck_objects: u64,
    pub collectable_objects: u64,
}

/// Totals over everything scanned.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub critical: u64,
    pub warnings: u64,
    pub info: u64,
    pub rows_scanned: u64,
    pub keys_listed: u64,
    pub listed_bytes: u64,
    pub pages: u64,
    /// Rows recorded in a different bucket than the store's; not compared.
    pub other_bucket_rows: u64,
    /// Rows created or verified after the listing started; counted, not compared.
    pub recent_rows: u64,
    /// Rows beyond the key range the listing covered; counted, not compared.
    pub unchecked_rows: u64,
    /// Listed keys beyond the key range the row scan covered; not classified.
    pub unchecked_keys: u64,
    /// Staging keys with an open reservation (an upload in flight).
    pub open_staging_keys: u64,
    pub staging_bytes: u64,
    pub orphan_bytes: u64,
}

/// Rows and bytes of one object kind.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct KindTotals {
    pub kind: String,
    /// Rows in any state but `deleted`, and their recorded bytes.
    pub live_rows: u64,
    pub live_bytes: u64,
    pub verified_rows: u64,
    pub verified_bytes: u64,
    /// Listed keys matched to a live row of this kind, and their stored (listed) bytes.
    pub stored_keys: u64,
    pub stored_bytes: u64,
}

/// The result of [`reconcile_tenant`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReconciliationReport {
    pub format_version: u32,
    pub tenant_id: TenantId,
    pub bucket: String,
    pub prefix: String,
    /// Database clock when the listing started, in Unix seconds.
    pub listing_started_unix: i64,
    /// `false` when any cap in `caps_reached` stopped the report early.
    pub complete: bool,
    pub caps_reached: Vec<Cap>,
    pub summary: Summary,
    pub counts: FindingCounts,
    pub kinds: Vec<KindTotals>,
    pub missing_objects: Vec<MissingObject>,
    pub orphan_keys: Vec<OrphanKey>,
    pub stale_staging_keys: Vec<StaleStagingKey>,
    pub size_mismatches: Vec<SizeMismatch>,
    pub stuck_objects: Vec<StuckObject>,
    pub collectable_objects: Vec<CollectableObject>,
}

impl ReconciliationReport {
    /// Whether the report is complete and found nothing critical or warning-level.
    #[must_use]
    pub fn clean(&self) -> bool {
        self.complete && self.summary.critical == 0 && self.summary.warnings == 0
    }
}

/// One listed key.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ListedKey {
    size: u64,
    last_modified: String,
}

/// One parsed listing page.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ListPage {
    keys: Vec<(String, ListedKey)>,
    next_token: Option<String>,
}

/// One `objects` row as scanned.
#[derive(Clone, Debug)]
struct Row {
    id: Uuid,
    kind: ObjectKind,
    in_store_bucket: bool,
    key: String,
    size: i64,
    state: RowState,
    created_age: i64,
    deleting_age: i64,
    recent: bool,
    references: i64,
    holds: i64,
}

type RowTuple = (String, String, String, String, i64, String, i64, i64, bool, i64, i64);

fn parse_kind(value: &str) -> Option<ObjectKind> {
    KINDS.into_iter().find(|kind| kind.as_str() == value)
}

/// Reconcile tenant `transaction.tenant_id()`'s rows with `store`'s bucket under
/// `tenants/<tenant>/`. Read-only: see the module documentation. The transaction is left
/// read-only.
pub async fn reconcile_tenant(
    transaction: &mut TenantTransaction<'_>,
    store: &ObjectStore,
    limits: &ReconciliationLimits,
) -> Result<ReconciliationReport, ReconciliationError> {
    limits.validate()?;
    sqlx::query("SET TRANSACTION READ ONLY").execute(transaction.connection()).await.map_err(|_| ReconciliationError::ReadOnly)?;
    let tenant = transaction.tenant_id();
    let prefix = format!("tenants/{tenant}/");
    let started: f64 = sqlx::query_scalar("SELECT extract(epoch FROM clock_timestamp())::float8")
        .fetch_one(transaction.connection())
        .await
        .map_err(database_error)?;
    if !started.is_finite() {
        return Err(ReconciliationError::CorruptRow);
    }
    let mut builder = Builder::new(tenant, store.bucket_name(), &prefix, limits, started);

    let listing = list_prefix(store, &prefix, limits).await?;
    builder.summary.pages = listing.pages;
    if let Some(cap) = listing.truncated_by {
        builder.cap(cap);
    }
    let rows = scan_rows(transaction, store.bucket_name(), started, limits.max_rows).await?;
    if rows.truncated {
        builder.cap(Cap::Rows);
    }

    // Comparisons stay within the key range both sides fully covered.
    let listing_horizon = listing.truncated_by.and_then(|_| listing.keys.keys().next_back().cloned());
    let row_horizon = if rows.truncated { rows.rows.last().map(|row| row.key.clone()) } else { None };
    let listed_covers = |key: &str| listing_horizon.as_deref().is_none_or(|last| key <= last);
    let rows_cover = |key: &str| row_horizon.as_deref().is_none_or(|last| key < last);

    let mut by_key: BTreeMap<&str, Vec<&Row>> = BTreeMap::new();
    for row in &rows.rows {
        by_key.entry(row.key.as_str()).or_default().push(row);
    }

    for row in &rows.rows {
        builder.row(row, listed_covers(&row.key), listing.keys.get(&row.key), limits.stuck_after);
    }

    // Listed keys with no live row: staging keys are checked against reservations, the rest are
    // orphans.
    let staging_prefix = format!("{prefix}staging/");
    let mut staging: Vec<(&str, &ListedKey)> = Vec::new();
    for (key, listed) in &listing.keys {
        builder.summary.keys_listed = builder.summary.keys_listed.saturating_add(1);
        builder.summary.listed_bytes = builder.summary.listed_bytes.saturating_add(listed.size);
        let rows_for_key = by_key.get(key.as_str());
        if rows_for_key.is_some_and(|rows| rows.iter().any(|row| row.in_store_bucket && row.state != RowState::Deleted)) {
            continue;
        }
        if !rows_cover(key) {
            builder.summary.unchecked_keys = builder.summary.unchecked_keys.saturating_add(1);
            continue;
        }
        if key.starts_with(&staging_prefix) && valid_key(key) {
            builder.summary.staging_bytes = builder.summary.staging_bytes.saturating_add(listed.size);
            staging.push((key.as_str(), listed));
            continue;
        }
        let reason = if !valid_key(key) {
            OrphanReason::UnsignableKey
        } else if rows_for_key.is_some_and(|rows| rows.iter().any(|row| row.in_store_bucket)) {
            OrphanReason::DeletedRowOnly
        } else {
            OrphanReason::NoRow
        };
        builder.orphan(key, listed, reason);
    }

    for chunk in staging.chunks(STAGING_LOOKUP_CHUNK) {
        let keys: Vec<String> = chunk.iter().map(|(key, _)| (*key).to_string()).collect();
        let found: Vec<(String, String, String, bool)> = sqlx::query_as(concat!(
            "SELECT staging_key, id::text, state, expires_at <= clock_timestamp() FROM upload_reservations ",
            "WHERE tenant_id = $1::uuid AND staging_key = ANY($2::text[])"
        ))
        .bind(tenant.to_string())
        .bind(&keys)
        .fetch_all(transaction.connection())
        .await
        .map_err(database_error)?;
        let reservations: HashMap<String, (String, String, bool)> =
            found.into_iter().map(|(key, id, state, expired)| (key, (id, state, expired))).collect();
        for (key, listed) in chunk {
            let (reason, reservation_id) = match reservations.get(*key) {
                None => (StagingReason::NoReservation, None),
                Some((id, state, expired)) => {
                    let id: Uuid = id.parse().map_err(|_| ReconciliationError::CorruptRow)?;
                    match state.as_str() {
                        "reserved" if !expired => {
                            builder.summary.open_staging_keys = builder.summary.open_staging_keys.saturating_add(1);
                            continue;
                        }
                        "reserved" | "expired" => (StagingReason::ReservationExpired, Some(id)),
                        "completed" | "promoted" | "rejected" => (StagingReason::ReservationClosed, Some(id)),
                        _ => return Err(ReconciliationError::CorruptRow),
                    }
                }
            };
            builder.stale_staging(key, listed, reason, reservation_id);
        }
    }

    Ok(builder.finish())
}

/// Accumulates findings under the per-category cap.
struct Builder {
    report: ReconciliationReport,
    summary: Summary,
    max_findings: usize,
}

impl Builder {
    fn new(tenant: TenantId, bucket: &str, prefix: &str, limits: &ReconciliationLimits, started: f64) -> Self {
        // `started` is finite; the cast saturates.
        #[allow(clippy::cast_possible_truncation)]
        let listing_started_unix = started.floor() as i64;
        let kinds = KINDS.iter().map(|kind| KindTotals { kind: kind.as_str().to_string(), ..KindTotals::default() }).collect();
        Self {
            report: ReconciliationReport {
                format_version: REPORT_FORMAT_VERSION,
                tenant_id: tenant,
                bucket: bucket.to_string(),
                prefix: prefix.to_string(),
                listing_started_unix,
                complete: true,
                caps_reached: Vec::new(),
                summary: Summary::default(),
                counts: FindingCounts::default(),
                kinds,
                missing_objects: Vec::new(),
                orphan_keys: Vec::new(),
                stale_staging_keys: Vec::new(),
                size_mismatches: Vec::new(),
                stuck_objects: Vec::new(),
                collectable_objects: Vec::new(),
            },
            summary: Summary::default(),
            max_findings: usize::try_from(limits.max_findings).unwrap_or(usize::MAX),
        }
    }

    fn cap(&mut self, cap: Cap) {
        if !self.report.caps_reached.contains(&cap) {
            self.report.caps_reached.push(cap);
        }
    }

    fn severity(&mut self, severity: Severity) {
        let counter = match severity {
            Severity::Critical => &mut self.summary.critical,
            Severity::Warning => &mut self.summary.warnings,
            Severity::Info => &mut self.summary.info,
        };
        *counter = counter.saturating_add(1);
    }

    /// Push `finding` unless the category is full; returns whether the cap was hit.
    fn push<T>(list: &mut Vec<T>, count: &mut u64, max: usize, finding: T) -> bool {
        *count = count.saturating_add(1);
        if list.len() < max {
            list.push(finding);
            false
        } else {
            true
        }
    }

    fn row(&mut self, row: &Row, listed_covers: bool, listed: Option<&ListedKey>, stuck_after: Duration) {
        self.summary.rows_scanned = self.summary.rows_scanned.saturating_add(1);
        let kind = row.kind.as_str().to_string();
        if let Some(totals) = self.report.kinds.iter_mut().find(|totals| totals.kind == kind) {
            let bytes = u64::try_from(row.size).unwrap_or(0);
            if row.state != RowState::Deleted {
                totals.live_rows = totals.live_rows.saturating_add(1);
                totals.live_bytes = totals.live_bytes.saturating_add(bytes);
                if let (true, Some(listed)) = (row.in_store_bucket, listed) {
                    totals.stored_keys = totals.stored_keys.saturating_add(1);
                    totals.stored_bytes = totals.stored_bytes.saturating_add(listed.size);
                }
            }
            if row.state == RowState::Verified {
                totals.verified_rows = totals.verified_rows.saturating_add(1);
                totals.verified_bytes = totals.verified_bytes.saturating_add(bytes);
            }
        }
        if !row.in_store_bucket {
            self.summary.other_bucket_rows = self.summary.other_bucket_rows.saturating_add(1);
            return;
        }
        let max = self.max_findings;
        let mut capped = false;

        // Stuck and collectable do not depend on the listing.
        let stuck_seconds = i64::try_from(stuck_after.as_secs()).unwrap_or(i64::MAX);
        let stuck_age = match row.state {
            RowState::Pending if row.created_age >= stuck_seconds => Some(row.created_age),
            RowState::Deleting if row.deleting_age >= stuck_seconds => Some(row.deleting_age),
            _ => None,
        };
        if let Some(age_seconds) = stuck_age {
            self.severity(Severity::Warning);
            let finding = StuckObject {
                severity: Severity::Warning,
                object_id: row.id,
                kind: kind.clone(),
                state: row.state,
                object_key: row.key.clone(),
                age_seconds,
                in_bucket: listed_covers.then_some(listed.is_some()),
            };
            capped |= Self::push(&mut self.report.stuck_objects, &mut self.report.counts.stuck_objects, max, finding);
        }
        if matches!(row.state, RowState::Pending | RowState::Verified | RowState::Rejected) && row.references == 0 && row.holds == 0 {
            self.severity(Severity::Info);
            let eligible = u64::try_from(row.created_age).is_ok_and(|age| age >= MIN_COLLECTION_AGE.as_secs());
            let finding = CollectableObject {
                severity: Severity::Info,
                object_id: row.id,
                kind: kind.clone(),
                state: row.state,
                object_key: row.key.clone(),
                size_bytes: row.size,
                age_seconds: row.created_age,
                eligible,
            };
            capped |= Self::push(&mut self.report.collectable_objects, &mut self.report.counts.collectable_objects, max, finding);
        }

        if !listed_covers {
            self.summary.unchecked_rows = self.summary.unchecked_rows.saturating_add(1);
        } else if row.recent {
            self.summary.recent_rows = self.summary.recent_rows.saturating_add(1);
        } else {
            match (listed, row.state) {
                (None, RowState::Verified | RowState::Pending) => {
                    let severity = if row.state == RowState::Verified { Severity::Critical } else { Severity::Info };
                    self.severity(severity);
                    let finding =
                        MissingObject { severity, object_id: row.id, kind, state: row.state, object_key: row.key.clone(), size_bytes: row.size };
                    capped |= Self::push(&mut self.report.missing_objects, &mut self.report.counts.missing_objects, max, finding);
                }
                (Some(listed), RowState::Pending | RowState::Verified | RowState::Rejected | RowState::Deleting)
                    if u64::try_from(row.size).ok() != Some(listed.size) =>
                {
                    let severity = if row.state == RowState::Verified { Severity::Critical } else { Severity::Warning };
                    self.severity(severity);
                    let finding = SizeMismatch {
                        severity,
                        object_id: row.id,
                        kind,
                        state: row.state,
                        object_key: row.key.clone(),
                        recorded_bytes: row.size,
                        stored_bytes: listed.size,
                    };
                    capped |= Self::push(&mut self.report.size_mismatches, &mut self.report.counts.size_mismatches, max, finding);
                }
                _ => {}
            }
        }
        if capped {
            self.cap(Cap::Findings);
        }
    }

    fn orphan(&mut self, key: &str, listed: &ListedKey, reason: OrphanReason) {
        self.summary.orphan_bytes = self.summary.orphan_bytes.saturating_add(listed.size);
        self.severity(Severity::Warning);
        let finding = OrphanKey {
            severity: Severity::Warning,
            reason,
            object_key: key.to_string(),
            stored_bytes: listed.size,
            last_modified: listed.last_modified.clone(),
        };
        if Self::push(&mut self.report.orphan_keys, &mut self.report.counts.orphan_keys, self.max_findings, finding) {
            self.cap(Cap::Findings);
        }
    }

    fn stale_staging(&mut self, key: &str, listed: &ListedKey, reason: StagingReason, reservation_id: Option<Uuid>) {
        self.severity(Severity::Warning);
        let finding = StaleStagingKey {
            severity: Severity::Warning,
            reason,
            object_key: key.to_string(),
            reservation_id,
            stored_bytes: listed.size,
            last_modified: listed.last_modified.clone(),
        };
        if Self::push(&mut self.report.stale_staging_keys, &mut self.report.counts.stale_staging_keys, self.max_findings, finding) {
            self.cap(Cap::Findings);
        }
    }

    fn finish(mut self) -> ReconciliationReport {
        self.report.caps_reached.sort();
        self.report.complete = self.report.caps_reached.is_empty();
        self.report.summary = self.summary;
        self.report
    }
}

struct RowScan {
    rows: Vec<Row>,
    truncated: bool,
}

/// Read the tenant's rows in `(object_key, id)` order (byte order, matching S3 listing order), at
/// most `max_rows` of them.
async fn scan_rows(transaction: &mut TenantTransaction<'_>, store_bucket: &str, started: f64, max_rows: u32) -> Result<RowScan, ReconciliationError> {
    let max_rows = usize::try_from(max_rows).unwrap_or(usize::MAX);
    let mut rows: Vec<Row> = Vec::new();
    let mut after_key = String::new();
    let mut after_id = Uuid::nil();
    loop {
        // One more than the cap tells a truncated scan from an exact one.
        let remaining = max_rows.saturating_sub(rows.len()).saturating_add(1);
        let limit = i64::try_from(remaining.min(usize::try_from(ROW_PAGE).unwrap_or(usize::MAX))).unwrap_or(i64::from(ROW_PAGE));
        let page: Vec<RowTuple> = sqlx::query_as(concat!(
            "SELECT o.id::text, o.kind, o.bucket, o.object_key, o.size_bytes, o.state, ",
            "GREATEST(0, extract(epoch FROM clock_timestamp() - o.created_at))::bigint, ",
            "COALESCE(GREATEST(0, extract(epoch FROM clock_timestamp() - o.deleted_at))::bigint, 0), ",
            "(o.created_at >= to_timestamp($4::float8) OR COALESCE(o.verified_at >= to_timestamp($4::float8), false)), ",
            "(SELECT count(*) FROM object_references r WHERE r.tenant_id = o.tenant_id AND r.object_id = o.id ",
            "AND r.owner_kind <> 'retention_hold'), ",
            "(SELECT count(*) FROM object_references r WHERE r.tenant_id = o.tenant_id AND r.object_id = o.id ",
            "AND r.owner_kind = 'retention_hold') ",
            "FROM objects o WHERE o.tenant_id = $1::uuid ",
            "AND (o.object_key COLLATE \"C\", o.id) > ($2::text COLLATE \"C\", $3::uuid) ",
            "ORDER BY o.object_key COLLATE \"C\", o.id LIMIT $5"
        ))
        .bind(transaction.tenant_id().to_string())
        .bind(&after_key)
        .bind(after_id.to_string())
        .bind(started)
        .bind(limit)
        .fetch_all(transaction.connection())
        .await
        .map_err(database_error)?;
        let fetched = page.len();
        for tuple in page {
            if rows.len() >= max_rows {
                return Ok(RowScan { rows, truncated: true });
            }
            let row = row(tuple, store_bucket)?;
            after_key.clone_from(&row.key);
            after_id = row.id;
            rows.push(row);
        }
        if i64::try_from(fetched).unwrap_or(i64::MAX) < limit {
            return Ok(RowScan { rows, truncated: false });
        }
    }
}

fn row(tuple: RowTuple, store_bucket: &str) -> Result<Row, ReconciliationError> {
    let (id, kind, bucket, key, size, state, created_age, deleting_age, recent, references, holds) = tuple;
    Ok(Row {
        id: id.parse().map_err(|_| ReconciliationError::CorruptRow)?,
        kind: parse_kind(&kind).ok_or(ReconciliationError::CorruptRow)?,
        in_store_bucket: bucket == store_bucket,
        key,
        size,
        state: RowState::parse(&state).ok_or(ReconciliationError::CorruptRow)?,
        created_age,
        deleting_age,
        recent,
        references,
        holds,
    })
}

struct Listing {
    keys: BTreeMap<String, ListedKey>,
    pages: u64,
    truncated_by: Option<Cap>,
}

/// List every key under `prefix`, within the key and page caps.
async fn list_prefix(store: &ObjectStore, prefix: &str, limits: &ReconciliationLimits) -> Result<Listing, ReconciliationError> {
    let max_keys = usize::try_from(limits.max_keys).unwrap_or(usize::MAX);
    let page_size = usize::try_from(limits.page_size).unwrap_or(1000);
    let mut keys = BTreeMap::new();
    let mut token: Option<String> = None;
    let mut pages = 0_u64;
    loop {
        if pages >= u64::from(limits.max_pages) {
            return Ok(Listing { keys, pages, truncated_by: Some(Cap::Pages) });
        }
        let body = fetch_page(store, prefix, page_size, token.as_deref()).await?;
        pages = pages.saturating_add(1);
        let page = parse_listing(&body, prefix, page_size)?;
        for (key, listed) in page.keys {
            if keys.len() >= max_keys {
                return Ok(Listing { keys, pages, truncated_by: Some(Cap::Keys) });
            }
            if keys.insert(key, listed).is_some() {
                return Err(ReconciliationError::MalformedListing);
            }
        }
        match page.next_token {
            None => return Ok(Listing { keys, pages, truncated_by: None }),
            Some(next) if token.as_deref() == Some(next.as_str()) => return Err(ReconciliationError::MalformedListing),
            Some(next) => token = Some(next),
        }
    }
}

async fn fetch_page(store: &ObjectStore, prefix: &str, page_size: usize, token: Option<&str>) -> Result<Vec<u8>, ReconciliationError> {
    let mut action = store.bucket.list_objects_v2(Some(&store.credentials));
    action.with_prefix(prefix);
    action.with_max_keys(page_size);
    if let Some(token) = token {
        action.with_continuation_token(token);
    }
    // A bucket-level action: `rusty-s3` signs the validated base URL and unwraps nothing.
    let url = rusty_s3::S3Action::sign(&action, SELF_SIGNED_TTL);
    let mut response = store
        .client
        .get(url.as_str())
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|_| ReconciliationError::Store(ObjectStoreError::Unreachable))?;
    match response.status().as_u16() {
        200 => {}
        401 | 403 => return Err(ReconciliationError::Store(ObjectStoreError::Unauthorized)),
        404 => return Err(ReconciliationError::Store(ObjectStoreError::BucketMissing)),
        _ => return Err(ReconciliationError::Store(ObjectStoreError::Unexpected)),
    }
    if response.content_length().is_some_and(|length| length > MAX_LIST_RESPONSE_BYTES as u64) {
        return Err(ReconciliationError::MalformedListing);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ReconciliationError::Store(ObjectStoreError::Unreachable))? {
        if body.len().saturating_add(chunk.len()) > MAX_LIST_RESPONSE_BYTES {
            return Err(ReconciliationError::MalformedListing);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn child<'a, 'input>(node: roxmltree::Node<'a, 'input>, name: &str) -> Option<roxmltree::Node<'a, 'input>> {
    node.children().find(|child| child.is_element() && child.tag_name().name() == name)
}

fn text<'a>(node: roxmltree::Node<'a, '_>) -> &'a str {
    node.text().unwrap_or("").trim()
}

/// Parse one `ListBucketResult` page: at most `page_size` keys, each percent-decoded when the
/// response says `EncodingType=url`, at most [`MAX_KEY_BYTES`] long and under `prefix`.
fn parse_listing(body: &[u8], prefix: &str, page_size: usize) -> Result<ListPage, ReconciliationError> {
    let malformed = ReconciliationError::MalformedListing;
    if body.len() > MAX_LIST_RESPONSE_BYTES {
        return Err(malformed);
    }
    let source = std::str::from_utf8(body).map_err(|_| malformed)?;
    let options = roxmltree::ParsingOptions { allow_dtd: false, nodes_limit: MAX_LIST_XML_NODES, ..roxmltree::ParsingOptions::default() };
    let document = roxmltree::Document::parse_with_options(source, options).map_err(|_| malformed)?;
    let root = document.root_element();
    if root.tag_name().name() != "ListBucketResult" {
        return Err(malformed);
    }
    let encoded = match child(root, "EncodingType").map(text) {
        None => false,
        Some(value) if value.eq_ignore_ascii_case("url") => true,
        Some(_) => return Err(malformed),
    };
    let truncated = match child(root, "IsTruncated").map(text) {
        None | Some("false") => false,
        Some("true") => true,
        Some(_) => return Err(malformed),
    };
    let mut keys = Vec::new();
    for contents in root.children().filter(|node| node.is_element() && node.tag_name().name() == "Contents") {
        if keys.len() >= page_size {
            return Err(malformed);
        }
        let raw = child(contents, "Key").and_then(|node| node.text()).ok_or(malformed)?;
        let key = if encoded { percent_decode(raw).ok_or(malformed)? } else { raw.to_string() };
        if key.is_empty() || key.len() > MAX_KEY_BYTES || !key.starts_with(prefix) || key.chars().any(char::is_control) {
            return Err(malformed);
        }
        let size = child(contents, "Size").map(text).ok_or(malformed)?;
        if size.is_empty() || size.len() > 20 || !size.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(malformed);
        }
        let size: u64 = size.parse().map_err(|_| malformed)?;
        let last_modified = child(contents, "LastModified").map(text).unwrap_or("");
        let last_modified = if last_modified.len() <= MAX_TIMESTAMP_BYTES
            && last_modified.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'.' | b'-' | b'+'))
        {
            last_modified.to_string()
        } else {
            String::new()
        };
        keys.push((key, ListedKey { size, last_modified }));
    }
    let next_token = if truncated {
        let token = child(root, "NextContinuationToken").map(text).ok_or(malformed)?;
        if token.is_empty() || token.len() > MAX_TOKEN_BYTES || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(malformed);
        }
        Some(token.to_string())
    } else {
        None
    };
    Ok(ListPage { keys, next_token })
}

/// Decode `%XX` escapes (and `+` as a space, as S3's URL encoding type writes spaces); `None` on a
/// broken escape or a result that is not UTF-8.
fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'%' => {
                let high = bytes.get(index.saturating_add(1)).and_then(|digit| char::from(*digit).to_digit(16))?;
                let low = bytes.get(index.saturating_add(2)).and_then(|digit| char::from(*digit).to_digit(16))?;
                decoded.push(u8::try_from(high * 16 + low).ok()?);
                index = index.saturating_add(3);
            }
            b'+' => {
                decoded.push(b' ');
                index = index.saturating_add(1);
            }
            _ => {
                decoded.push(byte);
                index = index.saturating_add(1);
            }
        }
    }
    String::from_utf8(decoded).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREFIX: &str = "tenants/ac000000-0000-4000-8000-000000000001/";

    fn page(contents: &str, tail: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>figcraft</Name>{contents}{tail}</ListBucketResult>"#
        )
    }

    fn entry(key: &str, size: &str) -> String {
        format!("<Contents><Key>{key}</Key><LastModified>2026-10-17T10:00:00.000Z</LastModified><ETag>\"x\"</ETag><Size>{size}</Size></Contents>")
    }

    #[test]
    fn parses_pages_tokens_and_encoded_keys() {
        let body = page(
            &format!("{}{}", entry(&format!("{PREFIX}assets/ab"), "7"), entry("tenants%2Fac000000-0000-4000-8000-000000000001%2Fstaging%2Fx", "0")),
            "<IsTruncated>true</IsTruncated><NextContinuationToken>tok-1</NextContinuationToken><EncodingType>url</EncodingType>",
        );
        let parsed = parse_listing(body.as_bytes(), PREFIX, 1000).unwrap();
        assert_eq!(parsed.next_token.as_deref(), Some("tok-1"));
        assert_eq!(parsed.keys.len(), 2);
        assert_eq!(parsed.keys[0].0, format!("{PREFIX}assets/ab"));
        assert_eq!(parsed.keys[0].1, ListedKey { size: 7, last_modified: "2026-10-17T10:00:00.000Z".into() });
        assert_eq!(parsed.keys[1].0, format!("{PREFIX}staging/x"));
        let last = parse_listing(page("", "<IsTruncated>false</IsTruncated>").as_bytes(), PREFIX, 1000).unwrap();
        assert_eq!(last, ListPage { keys: Vec::new(), next_token: None });
        // A hostile timestamp is dropped, not echoed.
        let odd = page(&format!("<Contents><Key>{PREFIX}a</Key><LastModified>&lt;script&gt;</LastModified><Size>1</Size></Contents>"), "");
        assert_eq!(parse_listing(odd.as_bytes(), PREFIX, 1000).unwrap().keys[0].1.last_modified, "");
    }

    #[test]
    fn refuses_malformed_or_hostile_listings() {
        let ok_entry = entry(&format!("{PREFIX}assets/ab"), "1");
        let cases = [
            String::new(),
            "not xml".into(),
            "<Error><Code>AccessDenied</Code></Error>".into(),
            page(&entry("tenants/other/assets/ab", "1"), ""),
            page(&entry(&format!("{PREFIX}a"), "-1"), ""),
            page(&entry(&format!("{PREFIX}a"), "99999999999999999999999"), ""),
            page(&entry(&format!("{PREFIX}a"), ""), ""),
            page("<Contents><Size>1</Size></Contents>", ""),
            page(&entry(&format!("{PREFIX}a%zz"), "1"), "<EncodingType>url</EncodingType>"),
            page(&entry(&format!("{PREFIX}a%ff"), "1"), "<EncodingType>url</EncodingType>"),
            page(&entry(&format!("{PREFIX}a%0a"), "1"), "<EncodingType>url</EncodingType>"),
            page(&entry(&format!("{PREFIX}{}", "a".repeat(MAX_KEY_BYTES)), "1"), ""),
            page(&ok_entry, "<IsTruncated>true</IsTruncated>"),
            page(&ok_entry, "<IsTruncated>maybe</IsTruncated>"),
            page(&ok_entry, "<EncodingType>base64</EncodingType>"),
            page(
                &ok_entry,
                &format!("<IsTruncated>true</IsTruncated><NextContinuationToken>{}</NextContinuationToken>", "t".repeat(MAX_TOKEN_BYTES + 1)),
            ),
            page(&ok_entry, "<IsTruncated>true</IsTruncated><NextContinuationToken>a b</NextContinuationToken>"),
            format!("<!DOCTYPE x [<!ENTITY e \"e\">]>{}", page(&ok_entry, "")),
        ];
        for body in cases {
            assert_eq!(parse_listing(body.as_bytes(), PREFIX, 1000), Err(ReconciliationError::MalformedListing), "{body}");
        }
        let two = page(&format!("{ok_entry}{ok_entry}"), "");
        assert_eq!(parse_listing(two.as_bytes(), PREFIX, 1), Err(ReconciliationError::MalformedListing));
        assert_eq!(parse_listing(&vec![b' '; MAX_LIST_RESPONSE_BYTES + 1], PREFIX, 1000), Err(ReconciliationError::MalformedListing));
        let many = page(&"<X/>".repeat(usize::try_from(MAX_LIST_XML_NODES).unwrap() + 1), "");
        assert_eq!(parse_listing(many.as_bytes(), PREFIX, 1000), Err(ReconciliationError::MalformedListing));
    }

    #[test]
    fn junk_listings_never_panic() {
        // Every truncation and single-byte mutation of a valid page, then deterministic junk.
        let valid = page(
            &format!("{}{}", entry(&format!("{PREFIX}assets/ab"), "7"), entry(&format!("{PREFIX}staging/x%41"), "3")),
            "<IsTruncated>true</IsTruncated><NextContinuationToken>tok</NextContinuationToken><EncodingType>url</EncodingType>",
        );
        let bytes = valid.as_bytes();
        for cut in 0..bytes.len() {
            let _ = parse_listing(&bytes[..cut], PREFIX, 1000);
        }
        for index in 0..bytes.len() {
            for replacement in [b'<', b'>', b'%', b'&', 0, 0xff, b'"', b'/'] {
                let mut mutated = bytes.to_vec();
                mutated[index] = replacement;
                let _ = parse_listing(&mutated, PREFIX, 1000);
            }
        }
        let alphabet = b"<>/%&;=\"' abcKeySizeContentsListBucketResult0123";
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for _ in 0..2000 {
            let length = usize::try_from(state % 256).unwrap();
            let junk: Vec<u8> = (0..length)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    alphabet[usize::try_from(state % u64::try_from(alphabet.len()).unwrap()).unwrap()]
                })
                .collect();
            let _ = parse_listing(&junk, PREFIX, 1000);
        }
    }

    #[test]
    fn percent_decoding_is_strict() {
        assert_eq!(percent_decode("a%2Fb%2fc+d").as_deref(), Some("a/b/c d"));
        assert_eq!(percent_decode("%e2%82%ac").as_deref(), Some("\u{20ac}"));
        for broken in ["%", "%4", "%g0", "%c3"] {
            assert_eq!(percent_decode(broken), None, "{broken}");
        }
    }

    #[test]
    fn limits_are_bounded() {
        let base = ReconciliationLimits::default();
        assert!(base.validate().is_ok());
        for limits in [
            ReconciliationLimits { max_rows: 0, ..base },
            ReconciliationLimits { max_rows: MAX_ROWS + 1, ..base },
            ReconciliationLimits { max_keys: 0, ..base },
            ReconciliationLimits { max_keys: MAX_KEYS + 1, ..base },
            ReconciliationLimits { max_pages: 0, ..base },
            ReconciliationLimits { max_pages: MAX_PAGES + 1, ..base },
            ReconciliationLimits { page_size: 0, ..base },
            ReconciliationLimits { page_size: MAX_PAGE_SIZE + 1, ..base },
            ReconciliationLimits { max_findings: 0, ..base },
            ReconciliationLimits { max_findings: MAX_FINDINGS + 1, ..base },
            ReconciliationLimits { stuck_after: Duration::ZERO, ..base },
            ReconciliationLimits { stuck_after: MAX_STUCK_AFTER + Duration::from_secs(1), ..base },
        ] {
            assert_eq!(limits.validate(), Err(ReconciliationError::InvalidLimits), "{limits:?}");
        }
    }
}

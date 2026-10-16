//! Object-reference accounting, retention holds and the race-safe start of object deletion.
//!
//! Every `objects` row is kept alive by its `object_references` rows. A reference names the owner
//! that needs the bytes (a document, a snapshot, a library version and so on); a *retention hold*
//! is a reference whose `owner_kind` is `retention_hold` and whose `owner_id` is the hold's own id.
//! An object with no reference and no hold that is older than [`MIN_COLLECTION_AGE`] is a
//! collection candidate ([`collectable_objects`]); [`begin_object_deletion`] moves one candidate
//! to `deleting`, after which the garbage collector (S3-015) may remove its bytes.
//!
//! # Who may place holds
//!
//! Retention holds are an operator and legal control (`docs/saas/data-tenancy-integration.md`,
//! "Deletion and retention"). Only operator support tooling and legal-hold workflows, acting under
//! an operator audit record they write themselves, may call [`place_retention_hold`] or
//! [`release_retention_hold`]. Tenant members, document editing, upload completion and the garbage
//! collector must never place or release holds, and no tenant-facing route maps onto these
//! functions. Each hold is recorded twice in one transaction: the `retention_hold` reference that
//! keeps the object alive, and a `retention_holds` row naming the principal that placed it and a
//! bounded reason (1..=[`MAX_HOLD_REASON_CHARS`] characters, no control characters), so release can
//! be audited. `retention_holds` is keyed by `(tenant_id, id)`, so a hold id names one hold on one
//! object: a legal matter covering many objects places one hold per object (naming the matter in
//! the reason) and releases them object by object. Holds may be placed on
//! `pending`, `verified` and `rejected` objects, because a hold preserves bytes rather than using
//! them (a rejected upload may be evidence); ordinary references may not.
//!
//! # Locking
//!
//! [`add_reference`] and [`place_retention_hold`] lock the object row (`FOR SHARE` and
//! `FOR UPDATE`) before inserting, and [`begin_object_deletion`] locks it `FOR UPDATE` before it
//! counts references in a *separate* statement. The two locks conflict, so whichever transaction
//! locks first wins: a reference added first makes the deletion see it (under READ COMMITTED each
//! statement takes a fresh snapshot after the lock wait) and refuse with `Referenced`; a deletion
//! marked first makes the reference see the `deleting` row and refuse with `InvalidState`. Under
//! REPEATABLE READ or SERIALIZABLE the loser fails with a serialization error, reported as
//! `Conflict`. No path can leave a reference on a `deleting` or `deleted` object.

use std::fmt;
use std::time::Duration;

use figcraft_cloud_domain::Uuid;

use crate::TenantTransaction;

/// The floor on how old an unreferenced object must be before it can be collected. Callers can ask
/// for a longer age but never a shorter one: a fresh `pending` object may still be mid-upload, and
/// its first reference may not be committed yet.
pub const MIN_COLLECTION_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// The cap on a requested minimum age (ten years), so the bound stays a small integer interval.
pub const MAX_COLLECTION_AGE: Duration = Duration::from_secs(10 * 366 * 24 * 60 * 60);
/// The most rows one [`list_references`] call returns.
pub const MAX_REFERENCE_PAGE: u32 = 1000;
/// The most candidates one [`collectable_objects`] call returns.
pub const MAX_COLLECTABLE_PAGE: u32 = 1000;
/// The most retention holds one object may carry.
pub const MAX_HOLDS_PER_OBJECT: i64 = 64;
/// The longest retention-hold reason, in characters after trimming (the `retention_holds` check).
pub const MAX_HOLD_REASON_CHARS: usize = 500;
/// The most rows one [`list_retention_holds`] call returns.
pub const MAX_HOLD_PAGE: u32 = 1000;

/// Who owns an ordinary reference, matching the `object_references.owner_kind` constraint minus
/// `retention_hold`, which only the hold functions write.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum OwnerKind {
    Document,
    DocumentSnapshot,
    Asset,
    LibraryVersion,
    PluginVersion,
    Export,
    Thumbnail,
}

impl OwnerKind {
    const ALL: [Self; 7] =
        [Self::Document, Self::DocumentSnapshot, Self::Asset, Self::LibraryVersion, Self::PluginVersion, Self::Export, Self::Thumbnail];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Document => "document",
            Self::DocumentSnapshot => "document_snapshot",
            Self::Asset => "asset",
            Self::LibraryVersion => "library_version",
            Self::PluginVersion => "plugin_version",
            Self::Export => "export",
            Self::Thumbnail => "thumbnail",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == value)
    }
}

/// The `owner_kind` of a retention hold.
const RETENTION_HOLD: &str = "retention_hold";

/// What keeps an object alive.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReferenceOwner {
    Owner(OwnerKind, Uuid),
    RetentionHold(Uuid),
}

impl ReferenceOwner {
    fn parse(kind: &str, id: &str) -> Option<Self> {
        let id: Uuid = id.parse().ok()?;
        if kind == RETENTION_HOLD { Some(Self::RetentionHold(id)) } else { OwnerKind::parse(kind).map(|kind| Self::Owner(kind, id)) }
    }
}

/// Fixed, non-leaking categories for reference accounting failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectReferenceError {
    /// No such object in the transaction's tenant.
    NotFound,
    /// The object's state does not permit the operation (a reference to a rejected, deleting or
    /// deleted object, a hold on a deleting or deleted object, or deleting a deleted one).
    InvalidState,
    /// The object still has references or holds, so it cannot be marked for deletion.
    Referenced,
    /// The object is younger than [`MIN_COLLECTION_AGE`].
    TooYoung,
    /// The object already carries [`MAX_HOLDS_PER_OBJECT`] holds.
    TooManyHolds,
    /// A page limit of zero or above its maximum.
    InvalidLimit,
    /// A unique violation or a serialization failure; the caller may retry.
    Conflict,
    Constraint,
    Database,
    CorruptRow,
}

impl fmt::Display for ObjectReferenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "object reference error: {self:?}")
    }
}

impl std::error::Error for ObjectReferenceError {}

fn database_error(error: sqlx::Error) -> ObjectReferenceError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23505" | "40001" | "40P01") => ObjectReferenceError::Conflict,
        Some("23503" | "23514") => ObjectReferenceError::Constraint,
        _ => ObjectReferenceError::Database,
    }
}

/// Whether an idempotent write changed anything.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Change {
    Applied,
    AlreadyApplied,
}

/// Lock the object row and return its state, or `NotFound`.
async fn lock_object_state(transaction: &mut TenantTransaction<'_>, id: Uuid, exclusive: bool) -> Result<String, ObjectReferenceError> {
    let query = if exclusive {
        "SELECT state FROM objects WHERE tenant_id = $1::uuid AND id = $2::uuid FOR UPDATE"
    } else {
        "SELECT state FROM objects WHERE tenant_id = $1::uuid AND id = $2::uuid FOR SHARE"
    };
    let state: Option<String> = sqlx::query_scalar(query)
        .bind(transaction.tenant_id().to_string())
        .bind(id.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(database_error)?;
    state.ok_or(ObjectReferenceError::NotFound)
}

async fn reference_exists(transaction: &mut TenantTransaction<'_>, id: Uuid, kind: &str, owner: Uuid) -> Result<bool, ObjectReferenceError> {
    sqlx::query_scalar(concat!(
        "SELECT EXISTS (SELECT 1 FROM object_references ",
        "WHERE tenant_id = $1::uuid AND object_id = $2::uuid AND owner_kind = $3 AND owner_id = $4::uuid)"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(id.to_string())
    .bind(kind)
    .bind(owner.to_string())
    .fetch_one(transaction.connection())
    .await
    .map_err(database_error)
}

async fn insert_reference(transaction: &mut TenantTransaction<'_>, id: Uuid, kind: &str, owner: Uuid) -> Result<Change, ObjectReferenceError> {
    let inserted = sqlx::query(concat!(
        "INSERT INTO object_references (tenant_id, object_id, owner_kind, owner_id) ",
        "VALUES ($1::uuid, $2::uuid, $3, $4::uuid) ON CONFLICT DO NOTHING"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(id.to_string())
    .bind(kind)
    .bind(owner.to_string())
    .execute(transaction.connection())
    .await
    .map_err(database_error)?
    .rows_affected();
    Ok(if inserted == 0 { Change::AlreadyApplied } else { Change::Applied })
}

async fn delete_reference(transaction: &mut TenantTransaction<'_>, id: Uuid, kind: &str, owner: Uuid) -> Result<Change, ObjectReferenceError> {
    let deleted = sqlx::query(concat!(
        "DELETE FROM object_references ",
        "WHERE tenant_id = $1::uuid AND object_id = $2::uuid AND owner_kind = $3 AND owner_id = $4::uuid"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(id.to_string())
    .bind(kind)
    .bind(owner.to_string())
    .execute(transaction.connection())
    .await
    .map_err(database_error)?
    .rows_affected();
    Ok(if deleted == 0 { Change::AlreadyApplied } else { Change::Applied })
}

/// Record that `owner` needs object `id`. Idempotent: an existing reference is `AlreadyApplied`
/// whatever the object's state now. A new reference is accepted only on a `pending` or `verified`
/// object; a `rejected`, `deleting` or `deleted` one is `InvalidState`.
pub async fn add_reference(transaction: &mut TenantTransaction<'_>, id: Uuid, kind: OwnerKind, owner: Uuid) -> Result<Change, ObjectReferenceError> {
    let state = lock_object_state(transaction, id, false).await?;
    if reference_exists(transaction, id, kind.as_str(), owner).await? {
        return Ok(Change::AlreadyApplied);
    }
    if !matches!(state.as_str(), "pending" | "verified") {
        return Err(ObjectReferenceError::InvalidState);
    }
    insert_reference(transaction, id, kind.as_str(), owner).await
}

/// Drop `owner`'s reference to object `id`. Idempotent: a missing reference is `AlreadyApplied`.
/// Holds are released only through [`release_retention_hold`].
pub async fn remove_reference(
    transaction: &mut TenantTransaction<'_>,
    id: Uuid,
    kind: OwnerKind,
    owner: Uuid,
) -> Result<Change, ObjectReferenceError> {
    delete_reference(transaction, id, kind.as_str(), owner).await
}

/// Whether `reason` is an acceptable retention-hold reason: 1..=[`MAX_HOLD_REASON_CHARS`]
/// characters once trimmed, with no control characters (so it renders on one audit line).
#[must_use]
pub fn valid_hold_reason(reason: &str) -> bool {
    let trimmed = reason.trim();
    !trimmed.is_empty() && trimmed.chars().count() <= MAX_HOLD_REASON_CHARS && !reason.chars().any(char::is_control)
}

/// Place retention hold `hold` on object `id` on behalf of principal `placed_by`, for `reason`
/// (operator and legal callers only; see the module documentation). The reference and the
/// `retention_holds` row are written in the caller's transaction. Idempotent: an existing hold on
/// this object is `AlreadyApplied` and keeps its original placer and reason. Refused with
/// `Constraint` for an invalid reason ([`valid_hold_reason`]) or an unknown principal, `Conflict`
/// when `hold` already names a hold on another object, `InvalidState` on a `deleting` or
/// `deleted` object and `TooManyHolds` past [`MAX_HOLDS_PER_OBJECT`].
pub async fn place_retention_hold(
    transaction: &mut TenantTransaction<'_>,
    id: Uuid,
    hold: Uuid,
    placed_by: Uuid,
    reason: &str,
) -> Result<Change, ObjectReferenceError> {
    if !valid_hold_reason(reason) {
        return Err(ObjectReferenceError::Constraint);
    }
    // Exclusive, so concurrent holds on one object count each other and the cap holds.
    let state = lock_object_state(transaction, id, true).await?;
    let recorded: Option<String> = sqlx::query_scalar("SELECT object_id::text FROM retention_holds WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(transaction.tenant_id().to_string())
        .bind(hold.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(database_error)?;
    let reference = reference_exists(transaction, id, RETENTION_HOLD, hold).await?;
    let here = recorded.as_deref().map(|object| object == id.to_string());
    match (here, reference) {
        // A hold reference with no record predates `retention_holds` (an unattributed hold).
        (Some(true) | None, true) => return Ok(Change::AlreadyApplied),
        // A record without its reference, or one naming another object, is never written here.
        (Some(true), false) | (Some(false), true) => return Err(ObjectReferenceError::CorruptRow),
        (None | Some(false), false) => {}
    }
    if !matches!(state.as_str(), "pending" | "verified" | "rejected") {
        return Err(ObjectReferenceError::InvalidState);
    }
    if here == Some(false) {
        return Err(ObjectReferenceError::Conflict);
    }
    if reference_count(transaction, id).await?.holds >= MAX_HOLDS_PER_OBJECT {
        return Err(ObjectReferenceError::TooManyHolds);
    }
    let principal_known: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM principals WHERE id = $1::uuid)")
        .bind(placed_by.to_string())
        .fetch_one(transaction.connection())
        .await
        .map_err(database_error)?;
    if !principal_known {
        return Err(ObjectReferenceError::Constraint);
    }
    sqlx::query(concat!(
        "INSERT INTO retention_holds (tenant_id, id, object_id, placed_by, reason) ",
        "VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, $5)"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(hold.to_string())
    .bind(id.to_string())
    .bind(placed_by.to_string())
    .bind(reason.trim())
    .execute(transaction.connection())
    .await
    .map_err(database_error)?;
    insert_reference(transaction, id, RETENTION_HOLD, hold).await
}

/// Release retention hold `hold` on object `id` (operator and legal callers only), removing both
/// its reference and its `retention_holds` row. Idempotent.
pub async fn release_retention_hold(transaction: &mut TenantTransaction<'_>, id: Uuid, hold: Uuid) -> Result<Change, ObjectReferenceError> {
    let records = sqlx::query("DELETE FROM retention_holds WHERE tenant_id = $1::uuid AND id = $2::uuid AND object_id = $3::uuid")
        .bind(transaction.tenant_id().to_string())
        .bind(hold.to_string())
        .bind(id.to_string())
        .execute(transaction.connection())
        .await
        .map_err(database_error)?
        .rows_affected();
    let reference = delete_reference(transaction, id, RETENTION_HOLD, hold).await?;
    Ok(if records == 0 && reference == Change::AlreadyApplied { Change::AlreadyApplied } else { Change::Applied })
}

/// One retention hold on an object, with who placed it and why.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetentionHold {
    pub hold: Uuid,
    pub object_id: Uuid,
    pub placed_by: Uuid,
    pub reason: String,
}

/// Up to `limit` (1..=[`MAX_HOLD_PAGE`]) of object `id`'s retention holds, oldest first, with
/// their placer and reason.
pub async fn list_retention_holds(transaction: &mut TenantTransaction<'_>, id: Uuid, limit: u32) -> Result<Vec<RetentionHold>, ObjectReferenceError> {
    let limit = page_limit(limit, MAX_HOLD_PAGE)?;
    let rows: Vec<(String, String, String)> = sqlx::query_as(concat!(
        "SELECT id::text, placed_by::text, reason FROM retention_holds WHERE tenant_id = $1::uuid AND object_id = $2::uuid ",
        "ORDER BY created_at, id LIMIT $3"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(id.to_string())
    .bind(limit)
    .fetch_all(transaction.connection())
    .await
    .map_err(database_error)?;
    rows.into_iter()
        .map(|(hold, placed_by, reason)| {
            Ok(RetentionHold {
                hold: hold.parse().map_err(|_| ObjectReferenceError::CorruptRow)?,
                object_id: id,
                placed_by: placed_by.parse().map_err(|_| ObjectReferenceError::CorruptRow)?,
                reason,
            })
        })
        .collect()
}

/// How many ordinary references and retention holds an object has.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReferenceCount {
    pub references: i64,
    pub holds: i64,
}

impl ReferenceCount {
    #[must_use]
    pub const fn is_unreferenced(self) -> bool {
        self.references == 0 && self.holds == 0
    }
}

/// Count object `id`'s references and holds in the transaction's tenant (zero for an unknown or
/// foreign object).
pub async fn reference_count(transaction: &mut TenantTransaction<'_>, id: Uuid) -> Result<ReferenceCount, ObjectReferenceError> {
    let (references, holds): (i64, i64) = sqlx::query_as(concat!(
        "SELECT count(*) FILTER (WHERE owner_kind <> 'retention_hold'), count(*) FILTER (WHERE owner_kind = 'retention_hold') ",
        "FROM object_references WHERE tenant_id = $1::uuid AND object_id = $2::uuid"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(id.to_string())
    .fetch_one(transaction.connection())
    .await
    .map_err(database_error)?;
    Ok(ReferenceCount { references, holds })
}

fn page_limit(limit: u32, maximum: u32) -> Result<i64, ObjectReferenceError> {
    if limit == 0 || limit > maximum {
        return Err(ObjectReferenceError::InvalidLimit);
    }
    Ok(i64::from(limit))
}

/// Up to `limit` (1..=[`MAX_REFERENCE_PAGE`]) of object `id`'s references and holds, oldest
/// first. [`reference_count`] gives the totals.
pub async fn list_references(transaction: &mut TenantTransaction<'_>, id: Uuid, limit: u32) -> Result<Vec<ReferenceOwner>, ObjectReferenceError> {
    let limit = page_limit(limit, MAX_REFERENCE_PAGE)?;
    let rows: Vec<(String, String)> = sqlx::query_as(concat!(
        "SELECT owner_kind, owner_id::text FROM object_references WHERE tenant_id = $1::uuid AND object_id = $2::uuid ",
        "ORDER BY created_at, owner_kind, owner_id LIMIT $3"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(id.to_string())
    .bind(limit)
    .fetch_all(transaction.connection())
    .await
    .map_err(database_error)?;
    rows.iter().map(|(kind, owner)| ReferenceOwner::parse(kind, owner).ok_or(ObjectReferenceError::CorruptRow)).collect()
}

/// An object the garbage collector may consider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CollectableObject {
    pub id: Uuid,
    pub bucket: String,
    pub object_key: String,
}

type CollectableRow = (String, String, String);

fn collectable(row: CollectableRow) -> Result<CollectableObject, ObjectReferenceError> {
    let (id, bucket, object_key) = row;
    Ok(CollectableObject { id: id.parse().map_err(|_| ObjectReferenceError::CorruptRow)?, bucket, object_key })
}

/// Whole seconds of the effective minimum age: `requested`, raised to [`MIN_COLLECTION_AGE`] and
/// capped at [`MAX_COLLECTION_AGE`].
fn effective_age_seconds(requested: Duration) -> i64 {
    let age = requested.clamp(MIN_COLLECTION_AGE, MAX_COLLECTION_AGE);
    i64::try_from(age.as_secs()).unwrap_or(i64::MAX)
}

/// Up to `limit` (1..=[`MAX_COLLECTABLE_PAGE`]) objects in the transaction's tenant that have no
/// reference and no hold, are `pending`, `verified` or `rejected`, and were created more than
/// `min_age` ago (never less than [`MIN_COLLECTION_AGE`]), oldest first. This is only a candidate
/// list: each one must still go through [`begin_object_deletion`], which re-checks under a lock.
pub async fn collectable_objects(
    transaction: &mut TenantTransaction<'_>,
    min_age: Duration,
    limit: u32,
) -> Result<Vec<CollectableObject>, ObjectReferenceError> {
    let limit = page_limit(limit, MAX_COLLECTABLE_PAGE)?;
    let rows: Vec<CollectableRow> = sqlx::query_as(concat!(
        "SELECT o.id::text, o.bucket, o.object_key FROM objects o ",
        "WHERE o.tenant_id = $1::uuid AND o.state IN ('pending', 'verified', 'rejected') ",
        "AND o.created_at < now() - $2::bigint * interval '1 second' ",
        "AND NOT EXISTS (SELECT 1 FROM object_references r WHERE r.tenant_id = o.tenant_id AND r.object_id = o.id) ",
        "ORDER BY o.created_at, o.id LIMIT $3"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(effective_age_seconds(min_age))
    .bind(limit)
    .fetch_all(transaction.connection())
    .await
    .map_err(database_error)?;
    rows.into_iter().map(collectable).collect()
}

/// The result of [`begin_object_deletion`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeletionStart {
    /// The object moved to `deleting` in this transaction.
    Marked(CollectableObject),
    /// The object was already `deleting` (an earlier, perhaps interrupted, collection).
    AlreadyDeleting(CollectableObject),
}

/// Move object `id` from `pending`, `verified` or `rejected` to `deleting` (setting
/// `deleted_at`), but only if it is older than [`MIN_COLLECTION_AGE`] and has no reference and no
/// hold once its row is locked. The row lock conflicts with [`add_reference`] and
/// [`place_retention_hold`], so a concurrent reference either commits first and makes this
/// `Referenced`, or waits and is then refused. A `deleted` object is `InvalidState`.
pub async fn begin_object_deletion(transaction: &mut TenantTransaction<'_>, id: Uuid) -> Result<DeletionStart, ObjectReferenceError> {
    let row: Option<(String, String, String, bool)> = sqlx::query_as(concat!(
        "SELECT bucket, object_key, state, created_at < now() - $3::bigint * interval '1 second' ",
        "FROM objects WHERE tenant_id = $1::uuid AND id = $2::uuid FOR UPDATE"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(id.to_string())
    .bind(effective_age_seconds(MIN_COLLECTION_AGE))
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    let Some((bucket, object_key, state, old_enough)) = row else {
        return Err(ObjectReferenceError::NotFound);
    };
    let object = CollectableObject { id, bucket, object_key };
    match state.as_str() {
        "deleting" => return Ok(DeletionStart::AlreadyDeleting(object)),
        "pending" | "verified" | "rejected" => {}
        _ => return Err(ObjectReferenceError::InvalidState),
    }
    if !old_enough {
        return Err(ObjectReferenceError::TooYoung);
    }
    // A separate statement, so under READ COMMITTED it sees every reference committed before the
    // lock above was granted.
    if !reference_count(transaction, id).await?.is_unreferenced() {
        return Err(ObjectReferenceError::Referenced);
    }
    let updated = sqlx::query(concat!(
        "UPDATE objects SET state = 'deleting', deleted_at = now() ",
        "WHERE tenant_id = $1::uuid AND id = $2::uuid AND state IN ('pending', 'verified', 'rejected')"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(id.to_string())
    .execute(transaction.connection())
    .await
    .map_err(database_error)?
    .rows_affected();
    if updated == 1 { Ok(DeletionStart::Marked(object)) } else { Err(ObjectReferenceError::InvalidState) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_kinds_round_trip_and_holds_parse_separately() {
        let id: Uuid = "a5000000-0000-4000-8000-000000000001".parse().unwrap();
        for kind in OwnerKind::ALL {
            assert_eq!(OwnerKind::parse(kind.as_str()), Some(kind));
            assert_eq!(ReferenceOwner::parse(kind.as_str(), &id.to_string()), Some(ReferenceOwner::Owner(kind, id)));
        }
        assert_eq!(OwnerKind::parse(RETENTION_HOLD), None);
        assert_eq!(ReferenceOwner::parse(RETENTION_HOLD, &id.to_string()), Some(ReferenceOwner::RetentionHold(id)));
    }

    #[test]
    fn junk_rows_never_parse_or_panic() {
        let junk =
            ["", " ", "Document", "document ", "retention", "retention_hold\0", "\u{fffd}", "x".repeat(4096).as_str(), "😀", "-1"].map(str::to_owned);
        for kind in &junk {
            assert_eq!(OwnerKind::parse(kind), None);
            for owner in &junk {
                assert_eq!(ReferenceOwner::parse(kind, owner), None);
            }
            assert_eq!(ReferenceOwner::parse("document", kind), None);
        }
    }

    #[test]
    fn hold_reasons_are_bounded_and_single_line() {
        assert!(valid_hold_reason("Legal matter 2026-17"));
        assert!(valid_hold_reason(&"é".repeat(MAX_HOLD_REASON_CHARS)));
        assert!(valid_hold_reason(&format!("  {}  ", "x".repeat(MAX_HOLD_REASON_CHARS))));
        for junk in ["", " ", "\t\n", "line\nbreak", "nul\0", "bell\u{7}", "\u{85}next"] {
            assert!(!valid_hold_reason(junk), "{junk:?}");
        }
        assert!(!valid_hold_reason(&"x".repeat(MAX_HOLD_REASON_CHARS + 1)));
        assert!(!valid_hold_reason(&"😀".repeat(MAX_HOLD_REASON_CHARS + 1)));
    }

    #[test]
    fn page_limits_are_bounded() {
        for maximum in [MAX_REFERENCE_PAGE, MAX_COLLECTABLE_PAGE, MAX_HOLD_PAGE] {
            assert_eq!(page_limit(0, maximum), Err(ObjectReferenceError::InvalidLimit));
            assert_eq!(page_limit(maximum + 1, maximum), Err(ObjectReferenceError::InvalidLimit));
            assert_eq!(page_limit(u32::MAX, maximum), Err(ObjectReferenceError::InvalidLimit));
            assert_eq!(page_limit(1, maximum), Ok(1));
            assert_eq!(page_limit(maximum, maximum), Ok(i64::from(maximum)));
        }
    }

    #[test]
    fn the_minimum_age_cannot_be_lowered_and_is_capped() {
        let floor = i64::try_from(MIN_COLLECTION_AGE.as_secs()).unwrap();
        let cap = i64::try_from(MAX_COLLECTION_AGE.as_secs()).unwrap();
        for requested in [Duration::ZERO, Duration::from_nanos(1), Duration::from_secs(60), MIN_COLLECTION_AGE - Duration::from_nanos(1)] {
            assert_eq!(effective_age_seconds(requested), floor);
        }
        assert_eq!(effective_age_seconds(MIN_COLLECTION_AGE * 2), floor * 2);
        assert_eq!(effective_age_seconds(Duration::MAX), cap);
    }
}

//! Team libraries: published versions, team grants and per-document usage.
//!
//! A library is created from a source document and optionally owned by a team
//! ([`create_library`]). Each publish ([`publish_library_version`]) validates the bytes as a
//! `.figcraft` archive ([`crate::validation::validate`] with [`MEDIA_FIGCRAFT`]), locks the library
//! row, takes the next version number (`max + 1`), writes the bytes to the content-addressed key
//! `tenants/<tenant>/libraries/<library>/<version>-<sha256>.figcraft`, registers the object and
//! verifies it by read-back, then records the `library_versions` row and a `library_version`
//! object reference so the bytes outlive every later change except an explicit version removal.
//! Version numbers are dense and never reused: concurrent publishers serialize on the library
//! row lock, and `UNIQUE (tenant_id, library_id, version)` is the backstop (`Conflict`).
//!
//! Teams other than the owner see a library through [`grant_library_to_team`] (`viewer` or
//! `publisher`); [`revoke_library_grant`] removes a grant. A document records which version of a
//! library it uses with [`record_library_usage`], one row per document and library, upserted when
//! the document moves to another version; [`usages_of_version`] lists the documents still on a
//! version, so a later removal can refuse to drop one that is in use.
//!
//! # Library states
//!
//! [`archive_library`] freezes a library: no new versions, grants or usages. Documents that
//! already use an archived library keep their row, and re-recording the version they already use
//! is `AlreadyApplied`, but they cannot switch versions or start using it. A `deleted` library
//! refuses everything except reads and revocations. Nothing here deletes a library; that is a
//! retention workflow (S3-015).
//!
//! # Who may publish
//!
//! Route handlers authorize first (S4); [`publish_library_version`] repeats a minimal check as a
//! backstop so a publisher can never write into a library their tenant role and teams do not
//! reach. The publisher must be an active principal and an active member of the transaction's
//! tenant, and one of: a tenant owner or admin; for a team-owned library, an active `team_manager`
//! or `editor` of the owning team or of a team granted `publisher`; for a library with no owning
//! team, a tenant `editor`.
//!
//! # Register-first
//!
//! The `pending` object row is registered and committed in a short transaction of its own on the
//! pool *before* the bytes are uploaded, so a publish that rolls back (or crashes) after the upload
//! leaves a `pending` row naming the bytes instead of bytes no row names. A retry of the same bytes
//! at the same version number reuses that row; otherwise garbage collection retires it after its
//! minimum age (S3-015), and [`crate::garbage_collection::collect_orphans`] is the backstop for
//! keys written by older code.

use std::fmt;

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, TenantId, Uuid};
use sha2::{Digest, Sha256};

use sqlx::PgPool;

use crate::TenantTransaction;
use crate::document_snapshots::{SnapshotStoreError, preregister_object, upload_and_confirm};
use crate::object_metadata::{NewObject, ObjectKind, ObjectRecord, ObjectRepositoryError, keys};
use crate::object_references::{ObjectReferenceError, OwnerKind, add_reference};
use crate::objects::{ObjectStore, ObjectStoreError, valid_key};
use crate::uploads::MAX_LIBRARY_BYTES;
use crate::validation::{Budgets, MEDIA_FIGCRAFT, Rejection, validate};

/// Most versions one library may hold, so version numbers and listings stay small.
pub const MAX_VERSIONS_PER_LIBRARY: i64 = 10_000;
/// Most rows one [`list_versions`], [`usages_of_version`] or [`list_grants`] call returns.
pub const MAX_LIBRARY_PAGE: u32 = 1000;
/// Most characters in a library name (after trimming), matching the table constraint.
pub const MAX_LIBRARY_NAME_CHARS: usize = 255;
/// The producer version recorded on objects this module writes.
pub const PRODUCER_VERSION: &str = concat!("figcraft-cloud-storage/", env!("CARGO_PKG_VERSION"));

/// Fixed, non-leaking categories for library failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LibraryError {
    /// The name is empty after trimming, too long or contains control characters.
    InvalidName,
    /// A page limit of zero or above [`MAX_LIBRARY_PAGE`].
    InvalidLimit,
    /// The published bytes exceed [`MAX_LIBRARY_BYTES`].
    TooLarge,
    /// The published bytes are not a valid `.figcraft` archive.
    Invalid(Rejection),
    /// No such library in the transaction's tenant.
    NotFound,
    /// No such library version in the transaction's tenant.
    VersionNotFound,
    /// No such team in the transaction's tenant.
    TeamNotFound,
    /// The team is not active.
    TeamNotActive,
    /// No such document in the transaction's tenant.
    DocumentNotFound,
    /// The document is not active (or, for a source document, is being deleted).
    DocumentNotActive,
    /// The principal may not publish to this library (see the module documentation).
    NotPublisher,
    /// The library is archived: it takes no new versions, grants or usages.
    Archived,
    /// The library is deleted.
    Deleted,
    /// The library already holds [`MAX_VERSIONS_PER_LIBRARY`] versions.
    TooManyVersions,
    /// The version's key is held by an object that cannot be filled, or cannot be signed.
    ObjectConflict,
    /// The read-back of the written bytes did not match.
    VerificationFailed,
    /// A unique violation or a serialization failure; the caller may retry.
    Conflict,
    Constraint,
    Database,
    CorruptRow,
    /// The object store failed in a way that says nothing about the bytes.
    Store(ObjectStoreError),
}

impl fmt::Display for LibraryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "library error: {self:?}")
    }
}

impl std::error::Error for LibraryError {}

fn database_error(error: sqlx::Error) -> LibraryError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23505" | "40001" | "40P01") => LibraryError::Conflict,
        Some("23503" | "23514") => LibraryError::Constraint,
        _ => LibraryError::Database,
    }
}

fn object_error(error: ObjectRepositoryError) -> LibraryError {
    match error {
        ObjectRepositoryError::Conflict => LibraryError::ObjectConflict,
        ObjectRepositoryError::Constraint => LibraryError::Constraint,
        ObjectRepositoryError::Database => LibraryError::Database,
        ObjectRepositoryError::CorruptRow => LibraryError::CorruptRow,
        ObjectRepositoryError::InvalidState => LibraryError::ObjectConflict,
        ObjectRepositoryError::Store(error) => LibraryError::Store(error),
    }
}

/// Map a register-first failure (shared with document snapshots) onto library categories.
fn staged_error(error: SnapshotStoreError) -> LibraryError {
    match error {
        SnapshotStoreError::Conflict => LibraryError::ObjectConflict,
        SnapshotStoreError::Rejected(_) => LibraryError::VerificationFailed,
        SnapshotStoreError::Store(error) => LibraryError::Store(error),
        SnapshotStoreError::Object(error) => object_error(error),
        SnapshotStoreError::CorruptRow => LibraryError::CorruptRow,
        SnapshotStoreError::InvalidInput => LibraryError::TooLarge,
        _ => LibraryError::Database,
    }
}

fn reference_error(error: ObjectReferenceError) -> LibraryError {
    match error {
        ObjectReferenceError::Conflict => LibraryError::Conflict,
        ObjectReferenceError::Constraint => LibraryError::Constraint,
        ObjectReferenceError::CorruptRow => LibraryError::CorruptRow,
        ObjectReferenceError::NotFound | ObjectReferenceError::InvalidState => LibraryError::ObjectConflict,
        _ => LibraryError::Database,
    }
}

/// Whether an idempotent write changed anything.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Change {
    Applied,
    AlreadyApplied,
}

/// A library's lifecycle state, matching the `libraries.state` constraint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LibraryState {
    Active,
    Archived,
    Deleted,
}

impl LibraryState {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "archived" => Some(Self::Archived),
            "deleted" => Some(Self::Deleted),
            _ => None,
        }
    }

    /// `Ok` for an active library, otherwise the error for writing to it.
    const fn writable(self) -> Result<(), LibraryError> {
        match self {
            Self::Active => Ok(()),
            Self::Archived => Err(LibraryError::Archived),
            Self::Deleted => Err(LibraryError::Deleted),
        }
    }
}

/// What a granted team may do with a library, matching the `library_team_grants.role` constraint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LibraryRole {
    Viewer,
    Publisher,
}

impl LibraryRole {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Publisher => "publisher",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        [Self::Viewer, Self::Publisher].into_iter().find(|role| role.as_str() == value)
    }
}

/// A `libraries` row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Library {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub owning_team_id: Option<Uuid>,
    pub source_document_id: Option<CloudDocumentId>,
    pub name: String,
    pub state: LibraryState,
}

/// A `library_versions` row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LibraryVersion {
    pub id: Uuid,
    pub library_id: Uuid,
    pub version: u64,
    pub object_id: Uuid,
    pub sha256: [u8; 32],
    pub published_by: PrincipalId,
    /// Whole Unix seconds.
    pub created_at: i64,
}

/// A published version and its verified object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishedVersion {
    pub version: LibraryVersion,
    pub object: ObjectRecord,
}

/// A `library_team_grants` row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LibraryGrant {
    pub library_id: Uuid,
    pub team_id: Uuid,
    pub role: LibraryRole,
}

/// A `library_usages` row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LibraryUsage {
    pub document_id: CloudDocumentId,
    pub library_id: Uuid,
    pub library_version_id: Uuid,
}

/// What [`record_library_usage`] did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageChange {
    /// The document had no usage of this library; one was recorded.
    Recorded,
    /// The document moved from `previous` to the given version.
    Switched { previous: Uuid },
    /// The document already used this version.
    AlreadyRecorded,
}

type LibraryRow = (String, String, Option<String>, Option<String>, String, String);
type VersionRow = (String, String, i64, String, Vec<u8>, String, i64);

/// The selected library columns, as a literal so every query stays a compile-time string.
macro_rules! library_columns {
    () => {
        "id::text, tenant_id::text, owning_team_id::text, source_document_id::text, name, state"
    };
}

/// The selected version columns.
macro_rules! version_columns {
    () => {
        "id::text, library_id::text, version, object_id::text, sha256, published_by::text, extract(epoch FROM created_at)::bigint"
    };
}

fn parse_id<T: std::str::FromStr>(value: &str) -> Result<T, LibraryError> {
    value.parse().map_err(|_| LibraryError::CorruptRow)
}

fn library(row: LibraryRow) -> Result<Library, LibraryError> {
    let (id, tenant_id, owning_team_id, source_document_id, name, state) = row;
    Ok(Library {
        id: parse_id(&id)?,
        tenant_id: parse_id(&tenant_id)?,
        owning_team_id: owning_team_id.as_deref().map(parse_id).transpose()?,
        source_document_id: source_document_id.as_deref().map(parse_id).transpose()?,
        name,
        state: LibraryState::parse(&state).ok_or(LibraryError::CorruptRow)?,
    })
}

fn version(row: VersionRow) -> Result<LibraryVersion, LibraryError> {
    let (id, library_id, version, object_id, sha256, published_by, created_at) = row;
    Ok(LibraryVersion {
        id: parse_id(&id)?,
        library_id: parse_id(&library_id)?,
        version: u64::try_from(version).map_err(|_| LibraryError::CorruptRow)?,
        object_id: parse_id(&object_id)?,
        sha256: sha256.try_into().map_err(|_| LibraryError::CorruptRow)?,
        published_by: parse_id(&published_by)?,
        created_at,
    })
}

/// The trimmed name, or `InvalidName` when it is empty, longer than [`MAX_LIBRARY_NAME_CHARS`]
/// characters or contains a control character.
fn library_name(name: &str) -> Result<&str, LibraryError> {
    let trimmed = name.trim();
    let length = trimmed.chars().count();
    if length == 0 || length > MAX_LIBRARY_NAME_CHARS || trimmed.chars().any(char::is_control) {
        return Err(LibraryError::InvalidName);
    }
    Ok(trimmed)
}

fn page_limit(limit: u32) -> Result<i64, LibraryError> {
    if limit == 0 || limit > MAX_LIBRARY_PAGE {
        return Err(LibraryError::InvalidLimit);
    }
    Ok(i64::from(limit))
}

/// Check that `team` is an active team of the transaction's tenant.
async fn require_active_team(transaction: &mut TenantTransaction<'_>, team: Uuid) -> Result<(), LibraryError> {
    let state: Option<String> = sqlx::query_scalar("SELECT state FROM teams WHERE tenant_id = $1::uuid AND id = $2::uuid FOR SHARE")
        .bind(transaction.tenant_id().to_string())
        .bind(team.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(database_error)?;
    match state.as_deref() {
        None => Err(LibraryError::TeamNotFound),
        Some("active") => Ok(()),
        Some(_) => Err(LibraryError::TeamNotActive),
    }
}

/// The state of `document` in the transaction's tenant, locked against concurrent state changes.
async fn document_state(transaction: &mut TenantTransaction<'_>, document: CloudDocumentId) -> Result<String, LibraryError> {
    let state: Option<String> = sqlx::query_scalar("SELECT state FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid FOR SHARE")
        .bind(transaction.tenant_id().to_string())
        .bind(document.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(database_error)?;
    state.ok_or(LibraryError::DocumentNotFound)
}

/// Lock `id`'s library row (`FOR UPDATE` when `exclusive`, otherwise `FOR SHARE`).
async fn lock_library(transaction: &mut TenantTransaction<'_>, id: Uuid, exclusive: bool) -> Result<Library, LibraryError> {
    let query = if exclusive {
        concat!("SELECT ", library_columns!(), " FROM libraries WHERE tenant_id = $1::uuid AND id = $2::uuid FOR UPDATE")
    } else {
        concat!("SELECT ", library_columns!(), " FROM libraries WHERE tenant_id = $1::uuid AND id = $2::uuid FOR SHARE")
    };
    let row: Option<LibraryRow> = sqlx::query_as(query)
        .bind(transaction.tenant_id().to_string())
        .bind(id.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(database_error)?;
    row.map(library).transpose()?.ok_or(LibraryError::NotFound)
}

/// Create an active library in the transaction's tenant, optionally owned by an active team and
/// sourced from a document that is not being deleted. Both must be in the same tenant; another
/// tenant's team or document is `TeamNotFound` or `DocumentNotFound`.
pub async fn create_library(
    transaction: &mut TenantTransaction<'_>,
    owning_team: Option<Uuid>,
    source_document: Option<CloudDocumentId>,
    name: &str,
) -> Result<Library, LibraryError> {
    let name = library_name(name)?;
    if let Some(team) = owning_team {
        require_active_team(transaction, team).await?;
    }
    if let Some(document) = source_document
        && matches!(document_state(transaction, document).await?.as_str(), "deleting" | "deleted")
    {
        return Err(LibraryError::DocumentNotActive);
    }
    let row: LibraryRow = sqlx::query_as(concat!(
        "INSERT INTO libraries (id, tenant_id, owning_team_id, source_document_id, name) ",
        "VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, $5) RETURNING ",
        library_columns!()
    ))
    .bind(Uuid::new_v4().to_string())
    .bind(transaction.tenant_id().to_string())
    .bind(owning_team.map(|team| team.to_string()))
    .bind(source_document.map(|document| document.to_string()))
    .bind(name)
    .fetch_one(transaction.connection())
    .await
    .map_err(database_error)?;
    library(row)
}

/// `id`'s library in the transaction's tenant.
pub async fn get_library(transaction: &mut TenantTransaction<'_>, id: Uuid) -> Result<Option<Library>, LibraryError> {
    let row: Option<LibraryRow> =
        sqlx::query_as(concat!("SELECT ", library_columns!(), " FROM libraries WHERE tenant_id = $1::uuid AND id = $2::uuid"))
            .bind(transaction.tenant_id().to_string())
            .bind(id.to_string())
            .fetch_optional(transaction.connection())
            .await
            .map_err(database_error)?;
    row.map(library).transpose()
}

/// Archive an active library. Idempotent; a deleted library is `Deleted`.
pub async fn archive_library(transaction: &mut TenantTransaction<'_>, id: Uuid) -> Result<Change, LibraryError> {
    match lock_library(transaction, id, true).await?.state {
        LibraryState::Archived => return Ok(Change::AlreadyApplied),
        LibraryState::Deleted => return Err(LibraryError::Deleted),
        LibraryState::Active => {}
    }
    sqlx::query("UPDATE libraries SET state = 'archived', updated_at = now() WHERE tenant_id = $1::uuid AND id = $2::uuid AND state = 'active'")
        .bind(transaction.tenant_id().to_string())
        .bind(id.to_string())
        .execute(transaction.connection())
        .await
        .map_err(database_error)?;
    Ok(Change::Applied)
}

/// Whether `publisher` may publish to `library` (see the module documentation).
async fn may_publish(transaction: &mut TenantTransaction<'_>, library: &Library, publisher: PrincipalId) -> Result<bool, LibraryError> {
    sqlx::query_scalar(concat!(
        "SELECT EXISTS (SELECT 1 FROM tenant_members member JOIN principals principal ON principal.id = member.principal_id ",
        "WHERE member.tenant_id = $1::uuid AND member.principal_id = $2::uuid AND member.state = 'active' AND principal.state = 'active' ",
        "AND (member.role IN ('tenant_owner', 'tenant_admin') ",
        "OR ($3::uuid IS NULL AND member.role = 'editor') ",
        "OR EXISTS (SELECT 1 FROM team_members team_member JOIN teams team ",
        "ON team.tenant_id = team_member.tenant_id AND team.id = team_member.team_id ",
        "WHERE team_member.tenant_id = $1::uuid AND team_member.principal_id = $2::uuid AND team_member.state = 'active' ",
        "AND team_member.role IN ('team_manager', 'editor') AND team.state = 'active' ",
        "AND (team.id = $3::uuid OR EXISTS (SELECT 1 FROM library_team_grants grant_row ",
        "WHERE grant_row.tenant_id = $1::uuid AND grant_row.library_id = $4::uuid AND grant_row.team_id = team.id ",
        "AND grant_row.role = 'publisher')))))"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(publisher.to_string())
    .bind(library.owning_team_id.map(|team| team.to_string()))
    .bind(library.id.to_string())
    .fetch_one(transaction.connection())
    .await
    .map_err(database_error)
}

/// Publish `bytes` as the next version of `library_id`.
///
/// The bytes are validated as a `.figcraft` archive before any SQL runs. The library row is then
/// locked `FOR UPDATE`, so concurrent publishers take consecutive version numbers; a publisher
/// that the backstop check refuses is `NotPublisher`, and an archived or deleted library is
/// `Archived` or `Deleted`. The object row is then registered `pending` and committed on `pool`
/// (register-first, see the module documentation), the bytes are written, and the row is locked
/// and verified by read-back before the version row and its `library_version` reference are
/// inserted, all in `transaction`.
///
/// `pool` lends a second connection while `transaction` holds the library row lock, so it needs at
/// least one connection more than the publishers that can queue on one library.
pub async fn publish_library_version(
    pool: &PgPool,
    transaction: &mut TenantTransaction<'_>,
    store: &ObjectStore,
    library_id: Uuid,
    publisher: PrincipalId,
    bytes: Vec<u8>,
) -> Result<PublishedVersion, LibraryError> {
    let size_bytes = i64::try_from(bytes.len()).map_err(|_| LibraryError::TooLarge)?;
    if size_bytes > MAX_LIBRARY_BYTES {
        return Err(LibraryError::TooLarge);
    }
    validate(MEDIA_FIGCRAFT, &bytes, &Budgets::default()).map_err(LibraryError::Invalid)?;
    let sha256: [u8; 32] = Sha256::digest(&bytes).into();

    let locked = lock_library(transaction, library_id, true).await?;
    locked.state.writable()?;
    if !may_publish(transaction, &locked, publisher).await? {
        return Err(LibraryError::NotPublisher);
    }
    let latest: i64 =
        sqlx::query_scalar("SELECT COALESCE(MAX(version), 0) FROM library_versions WHERE tenant_id = $1::uuid AND library_id = $2::uuid")
            .bind(transaction.tenant_id().to_string())
            .bind(library_id.to_string())
            .fetch_one(transaction.connection())
            .await
            .map_err(database_error)?;
    if latest >= MAX_VERSIONS_PER_LIBRARY {
        return Err(LibraryError::TooManyVersions);
    }
    let next = latest.checked_add(1).ok_or(LibraryError::TooManyVersions)?;
    let next_unsigned = u64::try_from(next).map_err(|_| LibraryError::CorruptRow)?;

    let object_key = keys::library(transaction.tenant_id(), library_id, next_unsigned, &sha256);
    if !valid_key(&object_key) {
        return Err(LibraryError::ObjectConflict);
    }
    let object = NewObject {
        id: Uuid::new_v4(),
        kind: ObjectKind::Library,
        object_key: object_key.clone(),
        sha256,
        size_bytes,
        media_type: MEDIA_FIGCRAFT.to_string(),
        producer_version: PRODUCER_VERSION.to_string(),
    };
    let (registered, _) = preregister_object(pool, transaction.tenant_id(), store.bucket_name(), &object).await.map_err(staged_error)?;
    let object = upload_and_confirm(transaction, store, &registered, bytes, MEDIA_FIGCRAFT).await.map_err(staged_error)?;

    let row: VersionRow = sqlx::query_as(concat!(
        "INSERT INTO library_versions (id, tenant_id, library_id, version, object_id, sha256, published_by) ",
        "VALUES ($1::uuid, $2::uuid, $3::uuid, $4, $5::uuid, $6, $7::uuid) RETURNING ",
        version_columns!()
    ))
    .bind(Uuid::new_v4().to_string())
    .bind(transaction.tenant_id().to_string())
    .bind(library_id.to_string())
    .bind(next)
    .bind(object.id.to_string())
    .bind(sha256.as_slice())
    .bind(publisher.to_string())
    .fetch_one(transaction.connection())
    .await
    .map_err(database_error)?;
    let version = version(row)?;
    add_reference(transaction, object.id, OwnerKind::LibraryVersion, version.id).await.map_err(reference_error)?;
    sqlx::query("UPDATE libraries SET updated_at = now() WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(transaction.tenant_id().to_string())
        .bind(library_id.to_string())
        .execute(transaction.connection())
        .await
        .map_err(database_error)?;
    Ok(PublishedVersion { version, object })
}

/// `id`'s library version in the transaction's tenant.
pub async fn get_version(transaction: &mut TenantTransaction<'_>, id: Uuid) -> Result<Option<LibraryVersion>, LibraryError> {
    let row: Option<VersionRow> =
        sqlx::query_as(concat!("SELECT ", version_columns!(), " FROM library_versions WHERE tenant_id = $1::uuid AND id = $2::uuid"))
            .bind(transaction.tenant_id().to_string())
            .bind(id.to_string())
            .fetch_optional(transaction.connection())
            .await
            .map_err(database_error)?;
    row.map(version).transpose()
}

/// Up to `limit` (1..=[`MAX_LIBRARY_PAGE`]) of `library`'s versions, newest first, strictly older
/// than `before` when given (keyset pagination by version number). An unknown or foreign library
/// lists nothing.
pub async fn list_versions(
    transaction: &mut TenantTransaction<'_>,
    library: Uuid,
    before: Option<u64>,
    limit: u32,
) -> Result<Vec<LibraryVersion>, LibraryError> {
    let limit = page_limit(limit)?;
    // Every stored version is below `i64::MAX`, so a larger cursor simply lists from the newest.
    let before = before.map_or(i64::MAX, |before| i64::try_from(before).unwrap_or(i64::MAX));
    let rows: Vec<VersionRow> = sqlx::query_as(concat!(
        "SELECT ",
        version_columns!(),
        " FROM library_versions WHERE tenant_id = $1::uuid AND library_id = $2::uuid AND version < $3 ",
        "ORDER BY version DESC LIMIT $4"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(library.to_string())
    .bind(before)
    .bind(limit)
    .fetch_all(transaction.connection())
    .await
    .map_err(database_error)?;
    rows.into_iter().map(version).collect()
}

/// Grant `team` `role` on `library`, or change an existing grant's role. Idempotent: the same role
/// again is `AlreadyApplied`. The library must be active and the team an active team of the same
/// tenant.
pub async fn grant_library_to_team(
    transaction: &mut TenantTransaction<'_>,
    library: Uuid,
    team: Uuid,
    role: LibraryRole,
) -> Result<Change, LibraryError> {
    lock_library(transaction, library, false).await?.state.writable()?;
    require_active_team(transaction, team).await?;
    let changed = sqlx::query(concat!(
        "INSERT INTO library_team_grants (tenant_id, library_id, team_id, role) VALUES ($1::uuid, $2::uuid, $3::uuid, $4) ",
        "ON CONFLICT (tenant_id, library_id, team_id) DO UPDATE SET role = EXCLUDED.role, updated_at = now() ",
        "WHERE library_team_grants.role <> EXCLUDED.role"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(library.to_string())
    .bind(team.to_string())
    .bind(role.as_str())
    .execute(transaction.connection())
    .await
    .map_err(database_error)?
    .rows_affected();
    Ok(if changed == 0 { Change::AlreadyApplied } else { Change::Applied })
}

/// Remove `team`'s grant on `library`. Idempotent, and allowed in every library state.
pub async fn revoke_library_grant(transaction: &mut TenantTransaction<'_>, library: Uuid, team: Uuid) -> Result<Change, LibraryError> {
    let deleted = sqlx::query("DELETE FROM library_team_grants WHERE tenant_id = $1::uuid AND library_id = $2::uuid AND team_id = $3::uuid")
        .bind(transaction.tenant_id().to_string())
        .bind(library.to_string())
        .bind(team.to_string())
        .execute(transaction.connection())
        .await
        .map_err(database_error)?
        .rows_affected();
    Ok(if deleted == 0 { Change::AlreadyApplied } else { Change::Applied })
}

/// Up to `limit` (1..=[`MAX_LIBRARY_PAGE`]) of `library`'s team grants, oldest first.
pub async fn list_grants(transaction: &mut TenantTransaction<'_>, library: Uuid, limit: u32) -> Result<Vec<LibraryGrant>, LibraryError> {
    let limit = page_limit(limit)?;
    let rows: Vec<(String, String, String)> = sqlx::query_as(concat!(
        "SELECT library_id::text, team_id::text, role FROM library_team_grants WHERE tenant_id = $1::uuid AND library_id = $2::uuid ",
        "ORDER BY created_at, team_id LIMIT $3"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(library.to_string())
    .bind(limit)
    .fetch_all(transaction.connection())
    .await
    .map_err(database_error)?;
    rows.iter()
        .map(|(library_id, team_id, role)| {
            Ok(LibraryGrant {
                library_id: parse_id(library_id)?,
                team_id: parse_id(team_id)?,
                role: LibraryRole::parse(role).ok_or(LibraryError::CorruptRow)?,
            })
        })
        .collect()
}

/// Record that `document` uses `library_version`, replacing the version it used of the same
/// library before.
///
/// Re-recording the current version is `AlreadyRecorded` in any library state. Otherwise the
/// document must be active and the library active: an archived library keeps its existing usages
/// but takes no new ones and no switches (`Archived`), a deleted one is `Deleted`. A version or
/// document of another tenant is `VersionNotFound` or `DocumentNotFound`. The library row is
/// locked `FOR SHARE`, so a usage and a concurrent [`archive_library`] serialize.
pub async fn record_library_usage(
    transaction: &mut TenantTransaction<'_>,
    document: CloudDocumentId,
    library_version: Uuid,
) -> Result<UsageChange, LibraryError> {
    let version = get_version(transaction, library_version).await?.ok_or(LibraryError::VersionNotFound)?;
    let locked = lock_library(transaction, version.library_id, false).await?;
    let document_state = document_state(transaction, document).await?;
    let current: Option<String> = sqlx::query_scalar(concat!(
        "SELECT library_version_id::text FROM library_usages ",
        "WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND library_id = $3::uuid FOR UPDATE"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(document.to_string())
    .bind(version.library_id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    let current: Option<Uuid> = current.as_deref().map(parse_id).transpose()?;
    if current == Some(version.id) {
        return Ok(UsageChange::AlreadyRecorded);
    }
    locked.state.writable()?;
    if document_state != "active" {
        return Err(LibraryError::DocumentNotActive);
    }
    // A concurrent first usage of the same library by the same document is resolved by the
    // primary key: the later insert becomes an update of the earlier one (and reports `Recorded`).
    sqlx::query(concat!(
        "INSERT INTO library_usages (tenant_id, document_id, library_id, library_version_id) ",
        "VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid) ",
        "ON CONFLICT (tenant_id, document_id, library_id) DO UPDATE SET library_version_id = EXCLUDED.library_version_id, updated_at = now()"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(document.to_string())
    .bind(version.library_id.to_string())
    .bind(version.id.to_string())
    .execute(transaction.connection())
    .await
    .map_err(database_error)?;
    Ok(match current {
        Some(previous) => UsageChange::Switched { previous },
        None => UsageChange::Recorded,
    })
}

/// Stop recording that `document` uses `library`. Idempotent, and allowed in every state.
pub async fn remove_library_usage(transaction: &mut TenantTransaction<'_>, document: CloudDocumentId, library: Uuid) -> Result<Change, LibraryError> {
    let deleted = sqlx::query("DELETE FROM library_usages WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND library_id = $3::uuid")
        .bind(transaction.tenant_id().to_string())
        .bind(document.to_string())
        .bind(library.to_string())
        .execute(transaction.connection())
        .await
        .map_err(database_error)?
        .rows_affected();
    Ok(if deleted == 0 { Change::AlreadyApplied } else { Change::Applied })
}

/// The version of `library` that `document` uses, if any.
pub async fn usage_of_library(
    transaction: &mut TenantTransaction<'_>,
    document: CloudDocumentId,
    library: Uuid,
) -> Result<Option<LibraryUsage>, LibraryError> {
    let row: Option<String> = sqlx::query_scalar(
        "SELECT library_version_id::text FROM library_usages WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND library_id = $3::uuid",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(document.to_string())
    .bind(library.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    row.map(|version| Ok(LibraryUsage { document_id: document, library_id: library, library_version_id: parse_id(&version)? })).transpose()
}

/// Up to `limit` (1..=[`MAX_LIBRARY_PAGE`]) of the documents using `library_version`, ordered by
/// document id; pass the last document of a page as `after` to continue.
pub async fn usages_of_version(
    transaction: &mut TenantTransaction<'_>,
    library_version: Uuid,
    after: Option<CloudDocumentId>,
    limit: u32,
) -> Result<Vec<LibraryUsage>, LibraryError> {
    let limit = page_limit(limit)?;
    let rows: Vec<(String, String, String)> = sqlx::query_as(concat!(
        "SELECT document_id::text, library_id::text, library_version_id::text FROM library_usages ",
        "WHERE tenant_id = $1::uuid AND library_version_id = $2::uuid AND ($3::uuid IS NULL OR document_id > $3::uuid) ",
        "ORDER BY document_id LIMIT $4"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(library_version.to_string())
    .bind(after.map(|document| document.to_string()))
    .bind(limit)
    .fetch_all(transaction.connection())
    .await
    .map_err(database_error)?;
    rows.iter()
        .map(|(document, library, version)| {
            Ok(LibraryUsage { document_id: parse_id(document)?, library_id: parse_id(library)?, library_version_id: parse_id(version)? })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_trimmed_bounded_and_printable() {
        assert_eq!(library_name("  Icons  "), Ok("Icons"));
        assert_eq!(library_name(&"é".repeat(MAX_LIBRARY_NAME_CHARS)).map(str::len), Ok(MAX_LIBRARY_NAME_CHARS * 2));
        let long = "x".repeat(MAX_LIBRARY_NAME_CHARS + 1);
        for junk in ["", " ", "\t\n", "a\0b", "a\u{7}b", "line\nbreak", "\u{85}", long.as_str()] {
            assert_eq!(library_name(junk), Err(LibraryError::InvalidName), "{junk:?}");
        }
    }

    #[test]
    fn junk_rows_and_limits_are_refused_without_panicking() {
        for junk in ["", "ACTIVE", "active ", "archive", "\0", "deleting"] {
            assert_eq!(LibraryState::parse(junk), None);
            assert_eq!(LibraryRole::parse(junk), None);
        }
        for role in [LibraryRole::Viewer, LibraryRole::Publisher] {
            assert_eq!(LibraryRole::parse(role.as_str()), Some(role));
        }
        assert_eq!(page_limit(0), Err(LibraryError::InvalidLimit));
        assert_eq!(page_limit(MAX_LIBRARY_PAGE + 1), Err(LibraryError::InvalidLimit));
        assert_eq!(page_limit(u32::MAX), Err(LibraryError::InvalidLimit));
        assert_eq!(page_limit(MAX_LIBRARY_PAGE), Ok(i64::from(MAX_LIBRARY_PAGE)));
        let id = "a8000000-0000-4000-8000-000000000001".to_string();
        let digest = vec![0; 32];
        assert!(version((id.clone(), id.clone(), -1, id.clone(), digest.clone(), id.clone(), 0)).is_err());
        assert!(version((id.clone(), id.clone(), 1, id.clone(), vec![0; 31], id.clone(), 0)).is_err());
        assert!(version(("junk".into(), id.clone(), 1, id.clone(), digest.clone(), id.clone(), 0)).is_err());
        assert!(version((id.clone(), id.clone(), 1, id.clone(), digest, id.clone(), 0)).is_ok());
        assert!(library((id.clone(), id.clone(), Some("x".into()), None, "n".into(), "active".into())).is_err());
        assert!(library((id.clone(), id.clone(), None, None, "n".into(), "gone".into())).is_err());
        assert!(library((id.clone(), id, None, None, "n".into(), "archived".into())).is_ok());
    }

    #[test]
    fn producer_version_fits_the_objects_constraint() {
        assert!((1..=64).contains(&PRODUCER_VERSION.chars().count()));
        assert!(PRODUCER_VERSION.bytes().all(|byte| byte.is_ascii_graphic()));
    }
}

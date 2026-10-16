//! Validated, tenant-private plugin bundles and their manifests.
//!
//! [`install_plugin_version`] stores one version of a tenant's plugin. The plugin is named by its
//! publisher and name ([`PluginName`]); its `plugins` row is created on first install. The bundle
//! (`FCPLUGB1`, see `figcraft-plugin`'s `bundle` module) is checked by
//! [`crate::validation::validate`] as [`MEDIA_PLUGIN`], then its manifest JSON is read out here with
//! a separate bounded reader whose part lengths must agree with what validation accepted. The
//! manifest must be a JSON object with an `id`, a `name` and a `main`; if it carries a `version`, it
//! must be the version being installed, and every version of one plugin must carry the same `id`.
//! The bytes go to their content-addressed key ([`keys::plugin`]), are registered and verified by
//! reading them back, and the `plugin_versions` row (state `verified`, manifest as JSONB) and a
//! `plugin_version` object reference are written in the same transaction.
//!
//! Installing the identical bundle for an existing version is idempotent (`reused`); a different
//! bundle for an existing version is `Conflict`. [`withdraw_plugin_version`] hides a version from
//! new installs but keeps its row, reference and bytes, because the row still names the object
//! (a foreign key) and installed copies may need to be re-fetched for audit; releasing the bytes is
//! the deletion workflow's job (S3-015/S3-016). [`list_versions`] pages version summaries without
//! manifests, and [`get_manifest`] reads one manifest back.
//!
//! # Concurrency
//!
//! `plugins` has no unique `(tenant_id, publisher, name)` constraint, so plugin lookup-or-create
//! takes a transaction-scoped advisory lock keyed by tenant, publisher and name before it looks.
//! The plugin row is then locked `FOR UPDATE`, which serializes every install and withdrawal of one
//! plugin; a concurrent first install of the same version in another plugin row is impossible, and
//! any remaining unique violation is `Conflict` (retry). On any `Err` roll back: bytes written to
//! the store before a failure are left for the garbage collector's untracked-key sweep.

use std::fmt;

use figcraft_cloud_domain::Uuid;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::TenantTransaction;
use crate::object_metadata::{
    NewObject, ObjectKind, ObjectRecord, ObjectRepositoryError, Verification, get_object, keys, register_object, verify_object,
};
use crate::object_references::{Change, ObjectReferenceError, OwnerKind, add_reference};
use crate::objects::{ObjectStore, ObjectStoreError};
use crate::uploads::{MAX_PLUGIN_BYTES, valid_producer_version};
use crate::validation::{Budgets, MEDIA_PLUGIN, Rejection, Summary, validate};

/// Largest manifest stored, as UTF-8 JSON and as PostgreSQL renders it (`plugin_versions.manifest`).
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;
/// Longest publisher or plugin name in characters (the `plugins` constraints).
pub const MAX_PLUGIN_NAME_CHARS: usize = 255;
/// Longest manifest `id`, matching `figcraft_plugin::manifest`.
pub const MAX_MANIFEST_ID: usize = 128;
/// The most versions one [`list_versions`] call returns.
pub const MAX_VERSION_PAGE: u32 = 1000;

const BUNDLE_MAGIC: &[u8; 8] = b"FCPLUGB1";

/// Why a manifest was refused. Codes are fixed and never contain manifest text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManifestProblem {
    /// Not JSON (or not UTF-8, or nested too deeply for the parser).
    NotJson,
    /// Valid JSON that is not an object.
    NotObject,
    /// `id` is missing, not a string, or not 1–128 ASCII letters, digits, `.`, `-` or `_`.
    InvalidId,
    /// `name` is missing, not a string or blank.
    InvalidName,
    /// `main` is missing, not a string or blank.
    InvalidMain,
    /// `version` is present but not a string.
    InvalidVersion,
    /// A string or key contains U+0000, which PostgreSQL JSONB cannot store.
    NulCharacter,
    /// Larger than [`MAX_MANIFEST_BYTES`] once stored.
    TooLarge,
}

impl ManifestProblem {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotJson => "not_json",
            Self::NotObject => "not_object",
            Self::InvalidId => "invalid_id",
            Self::InvalidName => "invalid_name",
            Self::InvalidMain => "invalid_main",
            Self::InvalidVersion => "invalid_version",
            Self::NulCharacter => "nul_character",
            Self::TooLarge => "too_large",
        }
    }
}

/// Fixed, non-leaking categories for plugin storage failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginStoreError {
    /// The publisher or name is empty, longer than [`MAX_PLUGIN_NAME_CHARS`], padded with spaces or
    /// contains control characters.
    InvalidPluginName,
    /// The version is not a plugin key segment ([`keys::plugin_version_valid`]).
    InvalidVersion,
    /// The producer version is empty, too long or not printable ASCII.
    InvalidProducerVersion,
    /// A page limit of zero or above [`MAX_VERSION_PAGE`].
    InvalidLimit,
    /// The bundle is larger than [`MAX_PLUGIN_BYTES`].
    TooLarge,
    /// Content validation refused the bundle.
    InvalidBundle(Rejection),
    /// The bundle is valid but its manifest is not acceptable.
    InvalidManifest(ManifestProblem),
    /// The manifest's `version` is not the version being installed.
    VersionMismatch,
    /// The manifest's `id` differs from the `id` of the plugin's other versions.
    IdMismatch,
    /// No such plugin or version in the transaction's tenant.
    NotFound,
    /// The plugin is disabled or deleted, or the version was withdrawn.
    InvalidState,
    /// A different bundle is already stored for this version, or a concurrent write won a unique
    /// constraint (the caller may retry).
    Conflict,
    /// The final key is held by an object that is not verified and cannot be filled.
    ObjectConflict,
    /// The final key's read-back did not match the bytes just written.
    VerificationFailed,
    Constraint,
    Database,
    CorruptRow,
    /// The object store failed in a way that says nothing about the bytes.
    Store(ObjectStoreError),
}

impl fmt::Display for PluginStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "plugin store error: {self:?}")
    }
}

impl std::error::Error for PluginStoreError {}

fn database_error(error: sqlx::Error) -> PluginStoreError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23505" | "40001" | "40P01") => PluginStoreError::Conflict,
        Some("23503" | "23514") => PluginStoreError::Constraint,
        _ => PluginStoreError::Database,
    }
}

fn object_error(error: ObjectRepositoryError) -> PluginStoreError {
    match error {
        ObjectRepositoryError::Conflict => PluginStoreError::Conflict,
        ObjectRepositoryError::Constraint => PluginStoreError::Constraint,
        ObjectRepositoryError::Database => PluginStoreError::Database,
        ObjectRepositoryError::CorruptRow => PluginStoreError::CorruptRow,
        ObjectRepositoryError::InvalidState => PluginStoreError::ObjectConflict,
        ObjectRepositoryError::Store(error) => PluginStoreError::Store(error),
    }
}

fn reference_error(error: ObjectReferenceError) -> PluginStoreError {
    match error {
        ObjectReferenceError::NotFound | ObjectReferenceError::CorruptRow => PluginStoreError::CorruptRow,
        ObjectReferenceError::InvalidState | ObjectReferenceError::Referenced | ObjectReferenceError::TooYoung => PluginStoreError::ObjectConflict,
        ObjectReferenceError::Conflict => PluginStoreError::Conflict,
        ObjectReferenceError::Constraint | ObjectReferenceError::TooManyHolds | ObjectReferenceError::InvalidLimit => PluginStoreError::Constraint,
        ObjectReferenceError::Database => PluginStoreError::Database,
    }
}

// ---- names, versions and states ----

/// A tenant plugin's catalogue identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PluginName<'a> {
    pub publisher: &'a str,
    pub name: &'a str,
}

/// A publisher or plugin name: 1–255 characters, no control characters, no surrounding
/// whitespace (so two spellings that the table would trim alike cannot both exist).
#[must_use]
pub fn valid_plugin_name(value: &str) -> bool {
    let chars = value.chars().count();
    (1..=MAX_PLUGIN_NAME_CHARS).contains(&chars) && value.trim() == value && !value.chars().any(char::is_control)
}

/// A plugin's state, matching the `plugins.state` constraint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginState {
    Active,
    Disabled,
    Deleted,
}

impl PluginState {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "disabled" => Some(Self::Disabled),
            "deleted" => Some(Self::Deleted),
            _ => None,
        }
    }
}

/// A plugin version's state, matching the `plugin_versions.state` constraint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PluginVersionState {
    Pending,
    Verified,
    Rejected,
    Withdrawn,
}

impl PluginVersionState {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "verified" => Some(Self::Verified),
            "rejected" => Some(Self::Rejected),
            "withdrawn" => Some(Self::Withdrawn),
            _ => None,
        }
    }
}

/// A `plugins` row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginRecord {
    pub id: Uuid,
    pub publisher: String,
    pub name: String,
    pub state: PluginState,
}

/// A `plugin_versions` row without its manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginVersion {
    pub id: Uuid,
    pub plugin_id: Uuid,
    pub version: String,
    pub object_id: Uuid,
    pub sha256: [u8; 32],
    pub state: PluginVersionState,
}

type PluginRow = (String, String, String, String);
type VersionRow = (String, String, String, String, Vec<u8>, String);
/// A [`VersionRow`] followed by `manifest::text`.
type ManifestRow = (String, String, String, String, Vec<u8>, String, String);

/// The selected `plugins` columns, as a literal so every query stays a compile-time string.
macro_rules! plugin_columns {
    () => {
        "id::text, publisher, name, state"
    };
}

/// The selected `plugin_versions` columns (never the manifest).
macro_rules! version_columns {
    () => {
        "id::text, plugin_id::text, version, object_id::text, sha256, state"
    };
}

fn plugin_record(row: PluginRow) -> Result<PluginRecord, PluginStoreError> {
    let (id, publisher, name, state) = row;
    Ok(PluginRecord {
        id: id.parse().map_err(|_| PluginStoreError::CorruptRow)?,
        publisher,
        name,
        state: PluginState::parse(&state).ok_or(PluginStoreError::CorruptRow)?,
    })
}

fn version_record(row: VersionRow) -> Result<PluginVersion, PluginStoreError> {
    let (id, plugin_id, version, object_id, sha256, state) = row;
    Ok(PluginVersion {
        id: id.parse().map_err(|_| PluginStoreError::CorruptRow)?,
        plugin_id: plugin_id.parse().map_err(|_| PluginStoreError::CorruptRow)?,
        version,
        object_id: object_id.parse().map_err(|_| PluginStoreError::CorruptRow)?,
        sha256: sha256.try_into().map_err(|_| PluginStoreError::CorruptRow)?,
        state: PluginVersionState::parse(&state).ok_or(PluginStoreError::CorruptRow)?,
    })
}

// ---- bundle and manifest reading (pure, bounded) ----

/// The parts of a bundle this module needs, read independently of [`crate::validation`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BundleLayout<'a> {
    manifest: &'a [u8],
    module_bytes: u64,
    assets: u32,
}

fn read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    bytes.get(at..end)?.try_into().ok().map(u32::from_le_bytes)
}

/// Read the manifest, the module length and the asset count from a bundle, or `None` when the
/// bytes are not laid out as one. Every length is checked against what is left before it is used.
fn bundle_layout(bytes: &[u8]) -> Option<BundleLayout<'_>> {
    if !bytes.starts_with(BUNDLE_MAGIC) {
        return None;
    }
    let mut at = BUNDLE_MAGIC.len();
    let manifest_len = usize::try_from(read_u32(bytes, at)?).ok()?;
    at = at.checked_add(4)?;
    let manifest_end = at.checked_add(manifest_len)?;
    let manifest = bytes.get(at..manifest_end)?;
    at = manifest_end;
    let module_len = read_u32(bytes, at)?;
    at = at.checked_add(4)?.checked_add(usize::try_from(module_len).ok()?)?;
    let assets = read_u32(bytes, at)?;
    Some(BundleLayout { manifest, module_bytes: u64::from(module_len), assets })
}

/// A manifest accepted for storage: its parsed value (what is stored), its `id` and its `version`
/// when it carries one.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckedManifest {
    pub value: Value,
    pub id: String,
    pub version: Option<String>,
}

fn contains_nul(value: &Value) -> bool {
    // serde_json caps nesting at 128 levels while parsing, so this recursion is bounded.
    match value {
        Value::String(text) => text.contains('\0'),
        Value::Array(items) => items.iter().any(contains_nul),
        Value::Object(map) => map.iter().any(|(key, item)| key.contains('\0') || contains_nul(item)),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

fn valid_manifest_id(id: &str) -> bool {
    (1..=MAX_MANIFEST_ID).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

fn non_blank_string(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).is_some_and(|text| !text.trim().is_empty())
}

/// Check manifest JSON text: at most [`MAX_MANIFEST_BYTES`], a JSON object with a valid `id`, a
/// non-blank `name` and `main`, an optional string `version` (trimmed, as the plugin crate reads
/// it) and no U+0000 anywhere.
pub fn check_manifest(text: &[u8]) -> Result<CheckedManifest, ManifestProblem> {
    if text.len() > MAX_MANIFEST_BYTES {
        return Err(ManifestProblem::TooLarge);
    }
    let value: Value = serde_json::from_slice(text).map_err(|_| ManifestProblem::NotJson)?;
    let object = value.as_object().ok_or(ManifestProblem::NotObject)?;
    if contains_nul(&value) {
        return Err(ManifestProblem::NulCharacter);
    }
    let id = object.get("id").and_then(Value::as_str).filter(|id| valid_manifest_id(id)).ok_or(ManifestProblem::InvalidId)?;
    if !non_blank_string(object.get("name")) {
        return Err(ManifestProblem::InvalidName);
    }
    if !non_blank_string(object.get("main")) {
        return Err(ManifestProblem::InvalidMain);
    }
    let version = match object.get("version") {
        None => None,
        Some(Value::String(version)) if version.trim().is_empty() => None,
        Some(Value::String(version)) => Some(version.trim().to_string()),
        Some(_) => return Err(ManifestProblem::InvalidVersion),
    };
    let id = id.to_string();
    Ok(CheckedManifest { value, id, version })
}

/// Validate `bundle` as a plugin bundle within the plugin size cap and extract its manifest,
/// cross-checking the manifest, module and asset counts against what validation accepted.
pub fn check_bundle(bundle: &[u8]) -> Result<CheckedManifest, PluginStoreError> {
    let max = u64::try_from(MAX_PLUGIN_BYTES).unwrap_or(0);
    if u64::try_from(bundle.len()).unwrap_or(u64::MAX) > max {
        return Err(PluginStoreError::TooLarge);
    }
    let budgets = Budgets { max_input_bytes: max, ..Budgets::DEFAULT };
    let validated = validate(MEDIA_PLUGIN, bundle, &budgets).map_err(PluginStoreError::InvalidBundle)?;
    let Summary::Plugin { assets, module_bytes } = validated.summary else {
        return Err(PluginStoreError::InvalidBundle(Rejection::MediaMismatch));
    };
    let layout = bundle_layout(bundle).ok_or(PluginStoreError::InvalidBundle(Rejection::Malformed))?;
    if layout.assets != assets || layout.module_bytes != module_bytes {
        return Err(PluginStoreError::InvalidBundle(Rejection::Malformed));
    }
    check_manifest(layout.manifest).map_err(PluginStoreError::InvalidManifest)
}

// ---- repository ----

/// The result of [`install_plugin_version`]. `reused` is true when the identical bundle was
/// already installed for this version and nothing was written.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Installed {
    pub plugin: PluginRecord,
    pub version: PluginVersion,
    pub object: ObjectRecord,
    pub reused: bool,
}

/// Find the transaction tenant's live (not deleted) plugin called `name`, or create it `active`,
/// and lock its row. A disabled plugin is returned as it is.
async fn lock_or_create_plugin(transaction: &mut TenantTransaction<'_>, name: PluginName<'_>) -> Result<PluginRecord, PluginStoreError> {
    let tenant = transaction.tenant_id().to_string();
    // Names cannot contain control characters, so `\n` separates the parts unambiguously.
    let lock_key = format!("figcraft.plugin\n{tenant}\n{}\n{}", name.publisher, name.name);
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(lock_key)
        .execute(transaction.connection())
        .await
        .map_err(database_error)?;
    let found: Option<PluginRow> = sqlx::query_as(concat!(
        "SELECT ",
        plugin_columns!(),
        " FROM plugins WHERE tenant_id = $1::uuid AND publisher = $2 AND name = $3 AND state <> 'deleted' ",
        "ORDER BY created_at, id LIMIT 1 FOR UPDATE"
    ))
    .bind(&tenant)
    .bind(name.publisher)
    .bind(name.name)
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    if let Some(row) = found {
        return plugin_record(row);
    }
    let row: PluginRow = sqlx::query_as(concat!(
        "INSERT INTO plugins (id, tenant_id, publisher, name) VALUES ($1::uuid, $2::uuid, $3, $4) RETURNING ",
        plugin_columns!()
    ))
    .bind(Uuid::new_v4().to_string())
    .bind(&tenant)
    .bind(name.publisher)
    .bind(name.name)
    .fetch_one(transaction.connection())
    .await
    .map_err(database_error)?;
    plugin_record(row)
}

/// The transaction tenant's plugin `id`, locked `FOR UPDATE`.
async fn lock_plugin(transaction: &mut TenantTransaction<'_>, id: Uuid) -> Result<PluginRecord, PluginStoreError> {
    let row: Option<PluginRow> =
        sqlx::query_as(concat!("SELECT ", plugin_columns!(), " FROM plugins WHERE tenant_id = $1::uuid AND id = $2::uuid FOR UPDATE"))
            .bind(transaction.tenant_id().to_string())
            .bind(id.to_string())
            .fetch_optional(transaction.connection())
            .await
            .map_err(database_error)?;
    row.map(plugin_record).transpose()?.ok_or(PluginStoreError::NotFound)
}

async fn version_row(
    transaction: &mut TenantTransaction<'_>,
    plugin: Uuid,
    version: &str,
    lock: bool,
) -> Result<Option<PluginVersion>, PluginStoreError> {
    let query = if lock {
        concat!(
            "SELECT ",
            version_columns!(),
            " FROM plugin_versions WHERE tenant_id = $1::uuid AND plugin_id = $2::uuid AND version = $3 FOR UPDATE"
        )
    } else {
        concat!("SELECT ", version_columns!(), " FROM plugin_versions WHERE tenant_id = $1::uuid AND plugin_id = $2::uuid AND version = $3")
    };
    let row: Option<VersionRow> = sqlx::query_as(query)
        .bind(transaction.tenant_id().to_string())
        .bind(plugin.to_string())
        .bind(version)
        .fetch_optional(transaction.connection())
        .await
        .map_err(database_error)?;
    row.map(version_record).transpose()
}

/// Store `bundle` as `version` of the transaction tenant's plugin `name`, creating the plugin on
/// first install. See the module documentation for every check. Callers must have authorized the
/// installing principal to manage the tenant's plugins.
///
/// - The plugin must be `active`; a disabled one is `InvalidState`.
/// - The identical bundle for an existing verified version returns it with `reused`; for a
///   withdrawn version it is `InvalidState`; a different bundle is `Conflict`.
pub async fn install_plugin_version(
    transaction: &mut TenantTransaction<'_>,
    store: &ObjectStore,
    name: PluginName<'_>,
    version: &str,
    bundle: &[u8],
    producer_version: &str,
) -> Result<Installed, PluginStoreError> {
    if !valid_plugin_name(name.publisher) || !valid_plugin_name(name.name) {
        return Err(PluginStoreError::InvalidPluginName);
    }
    if !keys::plugin_version_valid(version) {
        return Err(PluginStoreError::InvalidVersion);
    }
    if !valid_producer_version(producer_version) {
        return Err(PluginStoreError::InvalidProducerVersion);
    }
    // Everything about the bytes is checked before any SQL runs.
    let manifest = check_bundle(bundle)?;
    if manifest.version.as_deref().is_some_and(|carried| carried != version) {
        return Err(PluginStoreError::VersionMismatch);
    }
    let sha256: [u8; 32] = Sha256::digest(bundle).into();
    let size_bytes = i64::try_from(bundle.len()).map_err(|_| PluginStoreError::TooLarge)?;
    let manifest_text = serde_json::to_string(&manifest.value).map_err(|_| PluginStoreError::InvalidManifest(ManifestProblem::NotJson))?;

    let plugin = lock_or_create_plugin(transaction, name).await?;
    if plugin.state != PluginState::Active {
        return Err(PluginStoreError::InvalidState);
    }
    if let Some(existing) = version_row(transaction, plugin.id, version, true).await? {
        if existing.sha256 != sha256 {
            return Err(PluginStoreError::Conflict);
        }
        if existing.state != PluginVersionState::Verified {
            return Err(PluginStoreError::InvalidState);
        }
        let object = get_object(transaction, existing.object_id).await.map_err(object_error)?.ok_or(PluginStoreError::CorruptRow)?;
        return Ok(Installed { plugin, version: existing, object, reused: true });
    }

    // Every version of one plugin carries the same manifest id.
    let other_id: bool = sqlx::query_scalar(concat!(
        "SELECT EXISTS (SELECT 1 FROM plugin_versions WHERE tenant_id = $1::uuid AND plugin_id = $2::uuid ",
        "AND manifest->>'id' IS DISTINCT FROM $3)"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(plugin.id.to_string())
    .bind(&manifest.id)
    .fetch_one(transaction.connection())
    .await
    .map_err(database_error)?;
    if other_id {
        return Err(PluginStoreError::IdMismatch);
    }
    // PostgreSQL renders JSONB with spaces and expanded numbers; measure what the constraint will.
    let stored_size: Option<i32> = sqlx::query_scalar("SELECT octet_length($1::jsonb::text)")
        .bind(&manifest_text)
        .fetch_one(transaction.connection())
        .await
        .map_err(database_error)?;
    if stored_size.and_then(|size| usize::try_from(size).ok()).is_none_or(|size| size > MAX_MANIFEST_BYTES) {
        return Err(PluginStoreError::InvalidManifest(ManifestProblem::TooLarge));
    }

    let object = store_bundle(transaction, store, plugin.id, version, bundle, sha256, size_bytes, producer_version).await?;
    let row: VersionRow = sqlx::query_as(concat!(
        "INSERT INTO plugin_versions (id, tenant_id, plugin_id, version, object_id, sha256, manifest, state) ",
        "VALUES ($1::uuid, $2::uuid, $3::uuid, $4, $5::uuid, $6, $7::jsonb, 'verified') RETURNING ",
        version_columns!()
    ))
    .bind(Uuid::new_v4().to_string())
    .bind(transaction.tenant_id().to_string())
    .bind(plugin.id.to_string())
    .bind(version)
    .bind(object.id.to_string())
    .bind(sha256.as_slice())
    .bind(&manifest_text)
    .fetch_one(transaction.connection())
    .await
    .map_err(database_error)?;
    let stored = version_record(row)?;
    add_reference(transaction, object.id, OwnerKind::PluginVersion, stored.id).await.map_err(reference_error)?;
    sqlx::query("UPDATE plugins SET updated_at = now() WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(transaction.tenant_id().to_string())
        .bind(plugin.id.to_string())
        .execute(transaction.connection())
        .await
        .map_err(database_error)?;
    Ok(Installed { plugin, version: stored, object, reused: false })
}

/// Find or write the verified plugin object at the bundle's content-addressed key.
#[allow(clippy::too_many_arguments)]
async fn store_bundle(
    transaction: &mut TenantTransaction<'_>,
    store: &ObjectStore,
    plugin: Uuid,
    version: &str,
    bundle: &[u8],
    sha256: [u8; 32],
    size_bytes: i64,
    producer_version: &str,
) -> Result<ObjectRecord, PluginStoreError> {
    let tenant = transaction.tenant_id();
    let object_key = keys::plugin(tenant, plugin, version, &sha256).ok_or(PluginStoreError::InvalidVersion)?;
    // A live row may already hold the key: a verified one with the same size is reused, a pending
    // one is filled and verified here, and a rejected or deleting one blocks the key until garbage
    // collection retires it.
    let live: Option<(String, String, i64, Vec<u8>)> = sqlx::query_as(concat!(
        "SELECT id::text, state, size_bytes, sha256 FROM objects WHERE tenant_id = $1::uuid AND bucket = $2 AND object_key = $3 ",
        "AND state <> 'deleted' FOR UPDATE"
    ))
    .bind(tenant.to_string())
    .bind(store.bucket_name())
    .bind(&object_key)
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    let object_id = match live {
        Some((id, state, size, digest)) if size == size_bytes && digest.as_slice() == sha256.as_slice() => {
            let id: Uuid = id.parse().map_err(|_| PluginStoreError::CorruptRow)?;
            match state.as_str() {
                "verified" => return get_object(transaction, id).await.map_err(object_error)?.ok_or(PluginStoreError::CorruptRow),
                "pending" => id,
                _ => return Err(PluginStoreError::ObjectConflict),
            }
        }
        Some(_) => return Err(PluginStoreError::ObjectConflict),
        None => {
            let object = NewObject {
                id: Uuid::new_v4(),
                kind: ObjectKind::Plugin,
                object_key: object_key.clone(),
                sha256,
                size_bytes,
                media_type: MEDIA_PLUGIN.to_string(),
                producer_version: producer_version.to_string(),
            };
            register_object(transaction, store.bucket_name(), &object).await.map_err(object_error)?.id
        }
    };
    store.put_object(&object_key, bundle.to_vec(), MEDIA_PLUGIN).await.map_err(PluginStoreError::Store)?;
    match verify_object(transaction, store, object_id).await.map_err(object_error)? {
        Some(Verification::Verified(object)) => Ok(object),
        Some(Verification::Rejected(..)) => Err(PluginStoreError::VerificationFailed),
        None => Err(PluginStoreError::CorruptRow),
    }
}

/// Withdraw `version` of the transaction tenant's plugin `plugin` so it is no longer offered for
/// install. Idempotent: an already withdrawn version is `AlreadyApplied`. The row, its object
/// reference and the bytes stay (see the module documentation).
pub async fn withdraw_plugin_version(transaction: &mut TenantTransaction<'_>, plugin: Uuid, version: &str) -> Result<Change, PluginStoreError> {
    if !keys::plugin_version_valid(version) {
        return Err(PluginStoreError::InvalidVersion);
    }
    lock_plugin(transaction, plugin).await?;
    let existing = version_row(transaction, plugin, version, true).await?.ok_or(PluginStoreError::NotFound)?;
    match existing.state {
        PluginVersionState::Withdrawn => return Ok(Change::AlreadyApplied),
        PluginVersionState::Verified => {}
        PluginVersionState::Pending | PluginVersionState::Rejected => return Err(PluginStoreError::InvalidState),
    }
    sqlx::query("UPDATE plugin_versions SET state = 'withdrawn' WHERE tenant_id = $1::uuid AND id = $2::uuid AND state = 'verified'")
        .bind(transaction.tenant_id().to_string())
        .bind(existing.id.to_string())
        .execute(transaction.connection())
        .await
        .map_err(database_error)?;
    Ok(Change::Applied)
}

/// The transaction tenant's plugin `id`.
pub async fn get_plugin(transaction: &mut TenantTransaction<'_>, id: Uuid) -> Result<Option<PluginRecord>, PluginStoreError> {
    let row: Option<PluginRow> = sqlx::query_as(concat!("SELECT ", plugin_columns!(), " FROM plugins WHERE tenant_id = $1::uuid AND id = $2::uuid"))
        .bind(transaction.tenant_id().to_string())
        .bind(id.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(database_error)?;
    row.map(plugin_record).transpose()
}

/// The transaction tenant's live (not deleted) plugin called `name`.
pub async fn find_plugin(transaction: &mut TenantTransaction<'_>, name: PluginName<'_>) -> Result<Option<PluginRecord>, PluginStoreError> {
    if !valid_plugin_name(name.publisher) || !valid_plugin_name(name.name) {
        return Err(PluginStoreError::InvalidPluginName);
    }
    let row: Option<PluginRow> = sqlx::query_as(concat!(
        "SELECT ",
        plugin_columns!(),
        " FROM plugins WHERE tenant_id = $1::uuid AND publisher = $2 AND name = $3 AND state <> 'deleted' ",
        "ORDER BY created_at, id LIMIT 1"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(name.publisher)
    .bind(name.name)
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    row.map(plugin_record).transpose()
}

/// Up to `limit` (1..=[`MAX_VERSION_PAGE`]) versions of the transaction tenant's plugin `plugin`,
/// oldest first, without manifests. Withdrawn versions are included only when asked for. An
/// unknown or foreign plugin has no versions.
pub async fn list_versions(
    transaction: &mut TenantTransaction<'_>,
    plugin: Uuid,
    include_withdrawn: bool,
    limit: u32,
) -> Result<Vec<PluginVersion>, PluginStoreError> {
    if limit == 0 || limit > MAX_VERSION_PAGE {
        return Err(PluginStoreError::InvalidLimit);
    }
    let rows: Vec<VersionRow> = sqlx::query_as(concat!(
        "SELECT ",
        version_columns!(),
        " FROM plugin_versions WHERE tenant_id = $1::uuid AND plugin_id = $2::uuid ",
        "AND (state = 'verified' OR ($3 AND state = 'withdrawn')) ORDER BY created_at, id LIMIT $4"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(plugin.to_string())
    .bind(include_withdrawn)
    .bind(i64::from(limit))
    .fetch_all(transaction.connection())
    .await
    .map_err(database_error)?;
    rows.into_iter().map(version_record).collect()
}

/// A stored manifest and the version it belongs to.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredManifest {
    pub version: PluginVersion,
    pub manifest: Value,
}

/// The manifest of `version` of the transaction tenant's plugin `plugin`, whatever the version's
/// state (callers offering installs must check [`PluginVersion::state`]).
pub async fn get_manifest(transaction: &mut TenantTransaction<'_>, plugin: Uuid, version: &str) -> Result<Option<StoredManifest>, PluginStoreError> {
    if !keys::plugin_version_valid(version) {
        return Err(PluginStoreError::InvalidVersion);
    }
    let row: Option<ManifestRow> = sqlx::query_as(concat!(
        "SELECT ",
        version_columns!(),
        ", manifest::text FROM plugin_versions WHERE tenant_id = $1::uuid AND plugin_id = $2::uuid AND version = $3"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(plugin.to_string())
    .bind(version)
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    let Some((id, plugin_id, version, object_id, sha256, state, text)) = row else {
        return Ok(None);
    };
    // The table bounds the text; a longer one means the constraint was bypassed.
    if text.len() > MAX_MANIFEST_BYTES {
        return Err(PluginStoreError::CorruptRow);
    }
    let manifest: Value = serde_json::from_str(&text).map_err(|_| PluginStoreError::CorruptRow)?;
    let version = version_record((id, plugin_id, version, object_id, sha256, state))?;
    Ok(Some(StoredManifest { version, manifest }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WASM: &[u8] = b"\0asm\x01\0\0\0";
    const MANIFEST: &[u8] = br#"{"id":"org.example.p","name":"P","version":"1.0.0","main":"p.wasm","api":1}"#;

    fn bundle(manifest: &[u8], module: &[u8], assets: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut out = BUNDLE_MAGIC.to_vec();
        for part in [manifest, module] {
            out.extend(u32::try_from(part.len()).unwrap().to_le_bytes());
            out.extend(part);
        }
        out.extend(u32::try_from(assets.len()).unwrap().to_le_bytes());
        for (name, data) in assets {
            for part in [*name, *data] {
                out.extend(u32::try_from(part.len()).unwrap().to_le_bytes());
                out.extend(part);
            }
        }
        out
    }

    #[test]
    fn valid_bundles_yield_their_manifest() {
        let checked = check_bundle(&bundle(MANIFEST, WASM, &[(b"icon.png", b"123")])).unwrap();
        assert_eq!(checked.id, "org.example.p");
        assert_eq!(checked.version.as_deref(), Some("1.0.0"));
        assert_eq!(checked.value["main"], "p.wasm");
        // No version, a blank one, and a padded one (the plugin crate trims it).
        for (text, version) in [
            (br#"{"id":"a","name":"A","main":"a.wasm"}"#.as_slice(), None),
            (br#"{"id":"a","name":"A","main":"a.wasm","version":" "}"#, None),
            (br#"{"id":"a","name":"A","main":"a.wasm","version":" 2.0.0 "}"#, Some("2.0.0")),
        ] {
            assert_eq!(check_bundle(&bundle(text, WASM, &[])).unwrap().version.as_deref(), version);
        }
    }

    #[test]
    fn manifests_are_refused_with_fixed_codes() {
        for (text, problem) in [
            (b"not json".as_slice(), ManifestProblem::NotJson),
            (b"", ManifestProblem::NotJson),
            (b"[1]", ManifestProblem::NotObject),
            (b"\"text\"", ManifestProblem::NotObject),
            (br#"{"name":"A","main":"a"}"#, ManifestProblem::InvalidId),
            (br#"{"id":"","name":"A","main":"a"}"#, ManifestProblem::InvalidId),
            (br#"{"id":"a b","name":"A","main":"a"}"#, ManifestProblem::InvalidId),
            (br#"{"id":7,"name":"A","main":"a"}"#, ManifestProblem::InvalidId),
            (br#"{"id":"a","main":"a"}"#, ManifestProblem::InvalidName),
            (br#"{"id":"a","name":"  ","main":"a"}"#, ManifestProblem::InvalidName),
            (br#"{"id":"a","name":"A"}"#, ManifestProblem::InvalidMain),
            (br#"{"id":"a","name":"A","main":"a","version":1}"#, ManifestProblem::InvalidVersion),
            (br#"{"id":"a","name":"A","main":"a","x":"\u0000"}"#, ManifestProblem::NulCharacter),
            (br#"{"id":"a","name":"A","main":"a","\u0000":1}"#, ManifestProblem::NulCharacter),
            (br#"{"id":"a","name":"A","main":"a","x":"\ud800"}"#, ManifestProblem::NotJson),
        ] {
            assert_eq!(check_manifest(text), Err(problem), "{}", String::from_utf8_lossy(text));
            assert_eq!(check_bundle(&bundle(text, WASM, &[])), Err(PluginStoreError::InvalidManifest(problem)));
        }
        let long_id = format!(r#"{{"id":"{}","name":"A","main":"a"}}"#, "a".repeat(MAX_MANIFEST_ID + 1));
        assert_eq!(check_manifest(long_id.as_bytes()), Err(ManifestProblem::InvalidId));
        // Deep nesting stops at the parser's recursion limit instead of the stack.
        let deep = format!(r#"{{"id":"a","name":"A","main":"a","x":{}{}}}"#, "[".repeat(10_000), "]".repeat(10_000));
        assert_eq!(check_manifest(deep.as_bytes()), Err(ManifestProblem::NotJson));
        assert_eq!(check_manifest(&vec![b' '; MAX_MANIFEST_BYTES + 1]), Err(ManifestProblem::TooLarge));
    }

    #[test]
    fn invalid_bundles_are_refused_before_the_manifest_is_read() {
        assert_eq!(check_bundle(b""), Err(PluginStoreError::InvalidBundle(Rejection::Empty)));
        assert_eq!(check_bundle(b"PK\x03\x04"), Err(PluginStoreError::InvalidBundle(Rejection::MediaMismatch)));
        assert_eq!(check_bundle(&bundle(MANIFEST, b"nope", &[])), Err(PluginStoreError::InvalidBundle(Rejection::Malformed)));
        let mut trailing = bundle(MANIFEST, WASM, &[]);
        trailing.push(0);
        assert_eq!(check_bundle(&trailing), Err(PluginStoreError::InvalidBundle(Rejection::TrailingBytes)));
        let oversize = vec![0; usize::try_from(MAX_PLUGIN_BYTES).unwrap() + 1];
        assert_eq!(check_bundle(&oversize), Err(PluginStoreError::TooLarge));
    }

    #[test]
    fn junk_bundles_never_panic_and_never_yield_a_manifest_validation_refused() {
        let base = bundle(MANIFEST, WASM, &[(b"a.txt", b"xyz")]);
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..20_000 {
            let mut bytes = base.clone();
            let edits = 1 + next() % 4;
            for _ in 0..edits {
                let roll = next();
                let at = usize::try_from(roll % u64::try_from(bytes.len().max(1)).unwrap()).unwrap();
                match roll % 4 {
                    0 if !bytes.is_empty() => bytes[at] = u8::try_from(next() % 256).unwrap(),
                    1 => bytes.truncate(at),
                    2 => bytes.insert(at.min(bytes.len()), u8::try_from(next() % 256).unwrap()),
                    _ => {
                        // Overwrite a length field with a hostile value.
                        let field = [8usize, 12 + MANIFEST.len(), 16 + MANIFEST.len() + WASM.len()][usize::try_from(next() % 3).unwrap()];
                        if let Some(slot) = bytes.get_mut(field..field + 4) {
                            slot.copy_from_slice(&[0xFF, 0xFF, 0xFF, 0x7F]);
                        }
                    }
                }
            }
            let result = check_bundle(&bytes);
            if validate(MEDIA_PLUGIN, &bytes, &Budgets::DEFAULT).is_err() {
                assert!(matches!(result, Err(PluginStoreError::InvalidBundle(_))), "round {round}: {result:?}");
            }
            if let Ok(checked) = result {
                assert!(valid_manifest_id(&checked.id));
            }
        }
        // Pure random bytes, with and without the magic.
        for len in 0..512 {
            let mut bytes: Vec<u8> = (0..len).map(|_| u8::try_from(next() % 256).unwrap()).collect();
            assert!(check_bundle(&bytes).is_err());
            if len >= 8 {
                bytes[..8].copy_from_slice(BUNDLE_MAGIC);
                assert!(check_bundle(&bytes).is_err());
                let _ = bundle_layout(&bytes);
            }
        }
    }

    #[test]
    fn layout_reader_is_bounds_checked() {
        assert_eq!(bundle_layout(b""), None);
        assert_eq!(bundle_layout(b"FCPLUGB1"), None);
        assert_eq!(bundle_layout(b"FCPLUGB1\xff\xff\xff\xff"), None);
        let bytes = bundle(MANIFEST, WASM, &[(b"a", b"b")]);
        let layout = bundle_layout(&bytes).unwrap();
        assert_eq!((layout.manifest, layout.module_bytes, layout.assets), (MANIFEST, 8, 1));
    }

    #[test]
    fn plugin_names_are_bounded_and_unpadded() {
        for name in ["Acme", "Tidy names", "Ünïcødé", &"x".repeat(MAX_PLUGIN_NAME_CHARS)] {
            assert!(valid_plugin_name(name), "{name}");
        }
        for name in ["", " Acme", "Acme ", "a\nb", "a\0b", "\t", &"x".repeat(MAX_PLUGIN_NAME_CHARS + 1)] {
            assert!(!valid_plugin_name(name), "{name:?}");
        }
    }

    #[test]
    fn states_parse_only_known_values() {
        assert_eq!(PluginState::parse("active"), Some(PluginState::Active));
        assert_eq!(PluginState::parse("ACTIVE"), None);
        assert_eq!(PluginVersionState::parse("withdrawn"), Some(PluginVersionState::Withdrawn));
        assert_eq!(PluginVersionState::parse(""), None);
    }
}

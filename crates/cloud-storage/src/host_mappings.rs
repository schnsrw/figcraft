//! Host-managed external id mappings (S6-002).
//!
//! A registered host maps its own tenant, workspace, project, document and principal identifiers
//! onto FigCraft ids. Unlike the raw [`crate::external_mappings`] primitives, these operations
//! require the host to be registered in the transaction's tenant, check that the internal id
//! really belongs to that tenant (row-level security makes other tenants' rows invisible), bound
//! the external id and the number of mappings per host, and never reveal why a lookup failed.
//! Mappings are unique per `(tenant, host, kind, external id)` by the table's primary key.

use std::fmt;

use figcraft_cloud_domain::Uuid;

use crate::TenantTransaction;
use crate::external_mappings::ExternalMappingRecord;

pub const MAX_EXTERNAL_ID_BYTES: usize = 512;
/// Mappings one host may hold in one tenant, across all kinds.
pub const MAX_MAPPINGS_PER_HOST: i64 = 100_000;
pub const MAX_LISTED_MAPPINGS: i64 = 500;

/// Resource kinds a host can map.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MappingKind {
    Tenant,
    Workspace,
    Project,
    Document,
    Principal,
}

impl MappingKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tenant => "tenant",
            Self::Workspace => "workspace",
            Self::Project => "project",
            Self::Document => "document",
            Self::Principal => "principal",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostMappingError {
    InvalidInput,
    /// The host is not registered here, or the internal resource is not in this tenant.
    NotFound,
    Conflict,
    TooMany,
    Database,
}

impl fmt::Display for HostMappingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "host mapping error: {self:?}")
    }
}

impl std::error::Error for HostMappingError {}

fn database_error(error: sqlx::Error) -> HostMappingError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23505") => HostMappingError::Conflict,
        Some("23503" | "23514" | "22001") => HostMappingError::InvalidInput,
        _ => HostMappingError::Database,
    }
}

fn valid_external_id(external_id: &str) -> bool {
    !external_id.is_empty() && external_id.len() <= MAX_EXTERNAL_ID_BYTES && !external_id.chars().any(char::is_control)
}

async fn host_exists(transaction: &mut TenantTransaction<'_>, system_id: &str) -> Result<bool, HostMappingError> {
    let found: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM host_systems WHERE tenant_id = $1::uuid AND system_id = $2")
        .bind(transaction.tenant_id().to_string())
        .bind(system_id)
        .fetch_optional(transaction.connection())
        .await
        .map_err(database_error)?;
    Ok(found.is_some())
}

/// Whether `internal_id` is a resource of `kind` in this transaction's tenant.
async fn internal_exists(transaction: &mut TenantTransaction<'_>, kind: MappingKind, internal_id: Uuid) -> Result<bool, HostMappingError> {
    if kind == MappingKind::Tenant {
        return Ok(internal_id.to_string() == transaction.tenant_id().to_string());
    }
    // Literal SQL per kind; the tenant predicate is explicit as well as enforced by row security.
    let sql = match kind {
        MappingKind::Workspace => "SELECT 1 FROM workspaces WHERE tenant_id = $1::uuid AND id = $2::uuid",
        MappingKind::Project => "SELECT 1 FROM projects WHERE tenant_id = $1::uuid AND id = $2::uuid",
        MappingKind::Document => "SELECT 1 FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid",
        MappingKind::Principal => "SELECT 1 FROM tenant_members WHERE tenant_id = $1::uuid AND principal_id = $2::uuid AND state = 'active'",
        MappingKind::Tenant => return Ok(false),
    };
    let found: Option<(i32,)> = sqlx::query_as(sql)
        .bind(transaction.tenant_id().to_string())
        .bind(internal_id.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(database_error)?;
    Ok(found.is_some())
}

type Row = (String, String, String, String, String);

fn record(row: Row) -> Result<ExternalMappingRecord, HostMappingError> {
    Ok(ExternalMappingRecord {
        tenant_id: row.0.parse().map_err(|_| HostMappingError::Database)?,
        system_id: row.1,
        resource_kind: row.2,
        external_id: row.3,
        internal_id: Uuid::parse_str(&row.4).map_err(|_| HostMappingError::Database)?,
    })
}

/// Map a host's external id onto an internal resource of this tenant. The host must be registered
/// here and the internal resource must exist here (otherwise [`HostMappingError::NotFound`], the
/// same answer for both, so a caller cannot probe other tenants). A duplicate natural key is
/// [`HostMappingError::Conflict`]: a mapping is never silently re-pointed.
pub async fn map_external_resource(
    transaction: &mut TenantTransaction<'_>,
    system_id: &str,
    kind: MappingKind,
    external_id: &str,
    internal_id: Uuid,
) -> Result<ExternalMappingRecord, HostMappingError> {
    if !valid_external_id(external_id) || internal_id.is_nil() {
        return Err(HostMappingError::InvalidInput);
    }
    if !host_exists(transaction, system_id).await? || !internal_exists(transaction, kind, internal_id).await? {
        return Err(HostMappingError::NotFound);
    }
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM external_mappings WHERE tenant_id = $1::uuid AND system_id = $2")
        .bind(transaction.tenant_id().to_string())
        .bind(system_id)
        .fetch_one(transaction.connection())
        .await
        .map_err(database_error)?;
    if held >= MAX_MAPPINGS_PER_HOST {
        return Err(HostMappingError::TooMany);
    }
    let row: Row = sqlx::query_as(
        "INSERT INTO external_mappings (tenant_id, system_id, resource_kind, external_id, internal_id) \
         VALUES ($1::uuid, $2, $3, $4, $5::uuid) \
         RETURNING tenant_id::text, system_id, resource_kind, external_id, internal_id::text",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(system_id)
    .bind(kind.as_str())
    .bind(external_id)
    .bind(internal_id.to_string())
    .fetch_one(transaction.connection())
    .await
    .map_err(database_error)?;
    record(row)
}

/// Resolve a host's external id to the internal id, in this tenant and for this host only.
/// Another host's mapping of the same external id is a different row and never matches.
pub async fn resolve_external_resource(
    transaction: &mut TenantTransaction<'_>,
    system_id: &str,
    kind: MappingKind,
    external_id: &str,
) -> Result<Option<Uuid>, HostMappingError> {
    if !valid_external_id(external_id) {
        return Ok(None);
    }
    let found: Option<(String,)> = sqlx::query_as(
        "SELECT internal_id::text FROM external_mappings \
         WHERE tenant_id = $1::uuid AND system_id = $2 AND resource_kind = $3 AND external_id = $4",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(system_id)
    .bind(kind.as_str())
    .bind(external_id)
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    found.map(|(id,)| Uuid::parse_str(&id).map_err(|_| HostMappingError::Database)).transpose()
}

/// The host's external ids for one internal resource (reverse lookup), at most
/// [`MAX_LISTED_MAPPINGS`].
pub async fn external_ids_for_internal(
    transaction: &mut TenantTransaction<'_>,
    system_id: &str,
    kind: MappingKind,
    internal_id: Uuid,
) -> Result<Vec<String>, HostMappingError> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT external_id FROM external_mappings \
         WHERE tenant_id = $1::uuid AND system_id = $2 AND resource_kind = $3 AND internal_id = $4::uuid \
         ORDER BY external_id LIMIT 500",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(system_id)
    .bind(kind.as_str())
    .bind(internal_id.to_string())
    .fetch_all(transaction.connection())
    .await
    .map_err(database_error)?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Remove one mapping. Returns whether it existed.
pub async fn unmap_external_resource(
    transaction: &mut TenantTransaction<'_>,
    system_id: &str,
    kind: MappingKind,
    external_id: &str,
) -> Result<bool, HostMappingError> {
    let result =
        sqlx::query("DELETE FROM external_mappings WHERE tenant_id = $1::uuid AND system_id = $2 AND resource_kind = $3 AND external_id = $4")
            .bind(transaction.tenant_id().to_string())
            .bind(system_id)
            .bind(kind.as_str())
            .bind(external_id)
            .execute(transaction.connection())
            .await
            .map_err(database_error)?;
    Ok(result.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use super::{MAX_EXTERNAL_ID_BYTES, valid_external_id};

    #[test]
    fn external_ids_are_bounded_printable_text() {
        assert!(valid_external_id("ext-1"));
        assert!(valid_external_id(&"a".repeat(MAX_EXTERNAL_ID_BYTES)));
        assert!(!valid_external_id(""));
        assert!(!valid_external_id(&"a".repeat(MAX_EXTERNAL_ID_BYTES + 1)));
        assert!(!valid_external_id("a\nb"));
        assert!(!valid_external_id("a\0b"));
    }
}

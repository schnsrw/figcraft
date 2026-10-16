//! External resource identifier mapping repository.
//!
//! Lets an embedding host (the "Embedded workspace" deployment mode) map its own external
//! resource identifiers onto FigCraft's internal UUIDs. Every query derives its tenant predicate
//! from `TenantTransaction`, never a caller-supplied tenant parameter.

use std::fmt;

use figcraft_cloud_domain::{TenantId, Uuid};

use crate::TenantTransaction;

/// Fixed, non-leaking categories for external mapping repository failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryError {
    Conflict,
    Constraint,
    Database,
    CorruptRow,
}

impl fmt::Display for RepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "external mapping repository error: {self:?}")
    }
}

impl std::error::Error for RepositoryError {}

fn database_error(error: sqlx::Error) -> RepositoryError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23505") => RepositoryError::Conflict,
        Some("23503" | "23514") => RepositoryError::Constraint,
        _ => RepositoryError::Database,
    }
}

/// A mapping from one external system's resource identifier to a FigCraft internal UUID, unique by
/// tenant, registered system, resource kind and external identifier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalMappingRecord {
    pub tenant_id: TenantId,
    pub system_id: String,
    pub resource_kind: String,
    pub external_id: String,
    pub internal_id: Uuid,
}

type ExternalMappingRow = (String, String, String, String, String);

fn external_mapping(row: ExternalMappingRow) -> Result<ExternalMappingRecord, RepositoryError> {
    Ok(ExternalMappingRecord {
        tenant_id: row.0.parse().map_err(|_| RepositoryError::CorruptRow)?,
        system_id: row.1,
        resource_kind: row.2,
        external_id: row.3,
        internal_id: Uuid::parse_str(&row.4).map_err(|_| RepositoryError::CorruptRow)?,
    })
}

/// Create an external mapping. A duplicate `(tenant_id, system_id, resource_kind, external_id)`
/// surfaces as `RepositoryError::Conflict` from the table's composite primary key.
pub async fn create_external_mapping(
    transaction: &mut TenantTransaction<'_>,
    system_id: &str,
    resource_kind: &str,
    external_id: &str,
    internal_id: Uuid,
) -> Result<ExternalMappingRecord, RepositoryError> {
    let row: ExternalMappingRow = sqlx::query_as(
        "INSERT INTO external_mappings (tenant_id, system_id, resource_kind, external_id, internal_id) \
         VALUES ($1::uuid, $2, $3, $4, $5::uuid) \
         RETURNING tenant_id::text, system_id, resource_kind, external_id, internal_id::text",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(system_id)
    .bind(resource_kind)
    .bind(external_id)
    .bind(internal_id.to_string())
    .fetch_one(transaction.connection())
    .await
    .map_err(database_error)?;
    external_mapping(row)
}

/// Look up an external mapping by its full natural key, scoped to this transaction's tenant.
pub async fn get_external_mapping(
    transaction: &mut TenantTransaction<'_>,
    system_id: &str,
    resource_kind: &str,
    external_id: &str,
) -> Result<Option<ExternalMappingRecord>, RepositoryError> {
    let row: Option<ExternalMappingRow> = sqlx::query_as(
        "SELECT tenant_id::text, system_id, resource_kind, external_id, internal_id::text \
         FROM external_mappings \
         WHERE tenant_id = $1::uuid AND system_id = $2 AND resource_kind = $3 AND external_id = $4",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(system_id)
    .bind(resource_kind)
    .bind(external_id)
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    row.map(external_mapping).transpose()
}

#[cfg(test)]
mod tests {
    use super::{RepositoryError, external_mapping};
    use figcraft_cloud_domain::{TenantId, Uuid};

    #[test]
    fn row_mapping_parses_well_formed_rows_and_rejects_corrupt_ones() {
        let tenant_id = TenantId::random();
        let internal_id = Uuid::new_v4();
        let record =
            external_mapping((tenant_id.to_string(), "host-a".to_owned(), "document".to_owned(), "ext-123".to_owned(), internal_id.to_string()))
                .expect("well-formed row parses");
        assert_eq!(record.tenant_id, tenant_id);
        assert_eq!(record.system_id, "host-a");
        assert_eq!(record.resource_kind, "document");
        assert_eq!(record.external_id, "ext-123");
        assert_eq!(record.internal_id, internal_id);

        assert_eq!(
            external_mapping(("not-a-uuid".to_owned(), "host-a".to_owned(), "document".to_owned(), "ext-123".to_owned(), internal_id.to_string())),
            Err(RepositoryError::CorruptRow)
        );
        assert_eq!(
            external_mapping((tenant_id.to_string(), "host-a".to_owned(), "document".to_owned(), "ext-123".to_owned(), "not-a-uuid".to_owned())),
            Err(RepositoryError::CorruptRow)
        );
    }
}

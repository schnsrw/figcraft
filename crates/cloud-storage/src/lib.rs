//! Durable metadata and object storage adapters for FigCraft Cloud.

/// Safe categories for database migration failures. Driver details are intentionally not exposed,
/// because they can contain connection metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MigrationError {
    Connect,
    Apply,
}

/// Safe categories for schema-version readiness failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchemaVersionError {
    Connect,
    MigrationLedgerMissing,
    ReadVersion,
    UnsupportedVersion,
}

/// Apply embedded SQL migrations with a PostgreSQL advisory lock.
///
/// This function is called only by the explicit `figcraft-cloud --migrate` command. Normal service
/// startup never mutates the schema.
pub async fn migrate_database(database_url: &str) -> Result<(), MigrationError> {
    let pool = sqlx::postgres::PgPoolOptions::new().max_connections(1).connect(database_url).await.map_err(|_| MigrationError::Connect)?;
    let mut migrator = sqlx::migrate!("./migrations");
    migrator.set_locking(true);
    migrator.run(&pool).await.map_err(|_| MigrationError::Apply)?;
    pool.close().await;
    Ok(())
}

/// Check the highest successfully applied migration against this binary's supported range.
pub async fn check_schema_version(database_url: &str, minimum: i64, maximum: i64) -> Result<i64, SchemaVersionError> {
    if minimum > maximum {
        return Err(SchemaVersionError::UnsupportedVersion);
    }
    let pool = sqlx::postgres::PgPoolOptions::new().max_connections(1).connect(database_url).await.map_err(|_| SchemaVersionError::Connect)?;
    let ledger_exists = sqlx::query_scalar::<_, bool>("SELECT to_regclass('public._sqlx_migrations') IS NOT NULL")
        .fetch_one(&pool)
        .await
        .map_err(|_| SchemaVersionError::ReadVersion)?;
    if !ledger_exists {
        return Err(SchemaVersionError::MigrationLedgerMissing);
    }
    let version = sqlx::query_scalar::<_, Option<i64>>("SELECT MAX(version) FROM public._sqlx_migrations WHERE success = TRUE")
        .fetch_one(&pool)
        .await
        .map_err(|_| SchemaVersionError::ReadVersion)?
        .unwrap_or(0);
    if !supports_version(version, minimum, maximum) {
        return Err(SchemaVersionError::UnsupportedVersion);
    }
    pool.close().await;
    Ok(version)
}

fn supports_version(version: i64, minimum: i64, maximum: i64) -> bool {
    minimum <= maximum && (minimum..=maximum).contains(&version)
}

#[cfg(test)]
mod tests {
    use super::supports_version;

    #[test]
    fn schema_version_range_is_inclusive_and_rejects_invalid_ranges() {
        assert!(supports_version(4, 4, 7));
        assert!(supports_version(7, 4, 7));
        assert!(!supports_version(3, 4, 7));
        assert!(!supports_version(8, 4, 7));
        assert!(!supports_version(4, 7, 4));
    }
}

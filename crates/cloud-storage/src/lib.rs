//! Durable metadata and object storage adapters for FigCraft Cloud.

/// Safe categories for database migration failures. Driver details are intentionally not exposed,
/// because they can contain connection metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MigrationError {
    Connect,
    Apply,
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

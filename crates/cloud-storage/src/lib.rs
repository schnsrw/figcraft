//! Durable metadata and object storage adapters for FigCraft Cloud.

use figcraft_cloud_domain::TenantId;
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

pub mod audit;
pub mod events;
pub mod external_mappings;
pub mod idempotency;
pub mod legacy_scan;
pub mod metadata;
pub mod snapshots;

/// A SQLx transaction bound to one trusted tenant identifier.
///
/// The tenant identifier is carried alongside the SQL transaction so repository methods can
/// derive their SQL predicates from the same value used for the transaction-local RLS setting.
pub struct TenantTransaction<'a> {
    tenant_id: TenantId,
    inner: Transaction<'a, Postgres>,
}

impl TenantTransaction<'_> {
    #[must_use]
    pub const fn tenant_id(&self) -> TenantId {
        self.tenant_id
    }

    /// Access the scoped connection for queries inside this transaction.
    pub fn connection(&mut self) -> &mut PgConnection {
        &mut self.inner
    }

    pub async fn commit(self) -> Result<(), sqlx::Error> {
        self.inner.commit().await
    }

    pub async fn rollback(self) -> Result<(), sqlx::Error> {
        self.inner.rollback().await
    }
}

/// Safe categories for tenant-scoped transaction setup failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TenantTransactionError {
    Begin,
    SetContext,
}

/// Begin a transaction with a tenant ID local to that transaction only.
///
/// Callers must resolve `tenant_id` from authenticated or trusted job context; this helper does
/// not authorize a tenant. Repository operations should use the returned transaction, never the
/// pool, so future RLS policies can read `figcraft.tenant_id`. PostgreSQL resets the setting on
/// commit or rollback, including when a dropped transaction is returned to the pool.
pub async fn begin_tenant_transaction(pool: &PgPool, tenant_id: TenantId) -> Result<TenantTransaction<'_>, TenantTransactionError> {
    let mut transaction = pool.begin().await.map_err(|_| TenantTransactionError::Begin)?;
    sqlx::query("SELECT set_config('figcraft.tenant_id', $1, true)")
        .bind(tenant_id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(|_| TenantTransactionError::SetContext)?;
    Ok(TenantTransaction { tenant_id, inner: transaction })
}

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

/// Safe categories for database role provisioning failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProvisionError {
    Connect,
    RequiresAdministrator,
    Apply,
}

/// Create or rotate the limited migration and runtime roles, and reconcile ownership/grants.
///
/// Run once before migrations so the migrator can create tables, and again afterward to grant
/// runtime access to newly created application tables. An administrator-only connection is used
/// only by this explicit command. Passwords enter as bound parameters; role DDL is assembled
/// server-side and never included in application diagnostics.
pub async fn provision_database_roles(admin_url: &str, migrator_password: &str, runtime_password: &str) -> Result<(), ProvisionError> {
    let pool = sqlx::postgres::PgPoolOptions::new().max_connections(1).connect(admin_url).await.map_err(|_| ProvisionError::Connect)?;
    let mut transaction = pool.begin().await.map_err(|_| ProvisionError::Apply)?;
    let superuser = sqlx::query_scalar::<_, bool>("SELECT rolsuper FROM pg_roles WHERE rolname = current_user")
        .fetch_one(&mut *transaction)
        .await
        .map_err(|_| ProvisionError::Apply)?;
    if !superuser {
        return Err(ProvisionError::RequiresAdministrator);
    }
    sqlx::query("SELECT set_config('figcraft.migrator_password', $1, true)")
        .bind(migrator_password)
        .execute(&mut *transaction)
        .await
        .map_err(|_| ProvisionError::Apply)?;
    sqlx::query("SELECT set_config('figcraft.runtime_password', $1, true)")
        .bind(runtime_password)
        .execute(&mut *transaction)
        .await
        .map_err(|_| ProvisionError::Apply)?;
    sqlx::query(
        r#"
        DO $roles$
        DECLARE
            table_name TEXT;
            database_name TEXT := current_database();
        BEGIN
            IF octet_length(current_setting('figcraft.migrator_password')) < 32
                OR octet_length(current_setting('figcraft.runtime_password')) < 32 THEN
                RAISE EXCEPTION 'database role passwords are too short';
            END IF;
            IF EXISTS (
                SELECT 1 FROM pg_auth_members membership
                JOIN pg_roles member_role ON member_role.oid = membership.member
                WHERE member_role.rolname IN ('figcraft_migrator', 'figcraft_runtime')
            ) THEN
                RAISE EXCEPTION 'cloud database roles must have no memberships';
            END IF;

            IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'figcraft_migrator') THEN
                EXECUTE format('ALTER ROLE figcraft_migrator LOGIN NOINHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS PASSWORD %L', current_setting('figcraft.migrator_password'));
            ELSE
                EXECUTE format('CREATE ROLE figcraft_migrator LOGIN NOINHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS PASSWORD %L', current_setting('figcraft.migrator_password'));
            END IF;
            IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'figcraft_runtime') THEN
                EXECUTE format('ALTER ROLE figcraft_runtime LOGIN NOINHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS PASSWORD %L', current_setting('figcraft.runtime_password'));
            ELSE
                EXECUTE format('CREATE ROLE figcraft_runtime LOGIN NOINHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS PASSWORD %L', current_setting('figcraft.runtime_password'));
            END IF;

            EXECUTE format('REVOKE ALL ON DATABASE %I FROM PUBLIC', database_name);
            EXECUTE format('REVOKE ALL ON DATABASE %I FROM figcraft_migrator, figcraft_runtime', database_name);
            EXECUTE format('GRANT CONNECT ON DATABASE %I TO figcraft_migrator, figcraft_runtime', database_name);
            REVOKE ALL ON SCHEMA public FROM PUBLIC;
            REVOKE ALL ON SCHEMA public FROM figcraft_migrator, figcraft_runtime;
            GRANT USAGE, CREATE ON SCHEMA public TO figcraft_migrator;
            GRANT USAGE ON SCHEMA public TO figcraft_runtime;
            REVOKE ALL ON ALL TABLES IN SCHEMA public FROM PUBLIC;
            REVOKE ALL ON ALL TABLES IN SCHEMA public FROM figcraft_runtime;

            FOR table_name IN
                SELECT tablename FROM pg_tables
                WHERE schemaname = 'public' AND tablename = ANY (ARRAY[
                    '_sqlx_migrations', 'principals', 'identity_links', 'tenants', 'tenant_members',
                    'teams', 'team_members', 'workspaces', 'workspace_members',
                    'workspace_team_grants', 'projects', 'documents', 'document_members',
                    'document_team_grants', 'document_events', 'document_snapshots', 'jobs',
                    'idempotency_keys', 'audit_events', 'external_mappings'
                ])
            LOOP
                EXECUTE format('ALTER TABLE public.%I OWNER TO figcraft_migrator', table_name);
                EXECUTE format('REVOKE ALL ON TABLE public.%I FROM figcraft_runtime', table_name);
                IF table_name = '_sqlx_migrations' THEN
                    EXECUTE format('GRANT SELECT ON TABLE public.%I TO figcraft_runtime', table_name);
                ELSIF table_name IN ('document_events', 'audit_events') THEN
                    EXECUTE format('GRANT SELECT, INSERT ON TABLE public.%I TO figcraft_runtime', table_name);
                ELSE
                    EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE public.%I TO figcraft_runtime', table_name);
                END IF;
            END LOOP;
        END
        $roles$
        "#,
    )
    .execute(&mut *transaction)
    .await
    .map_err(|_| ProvisionError::Apply)?;
    transaction.commit().await.map_err(|_| ProvisionError::Apply)?;
    pool.close().await;
    Ok(())
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

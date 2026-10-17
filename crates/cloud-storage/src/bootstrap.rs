//! Single-tenant bootstrap administrator (S4-003).
//!
//! A fresh single-tenant deployment has no owner. [`ensure_bootstrap`] issues a one-time bootstrap
//! token (43 URL-safe base64 characters from 32 random bytes; only its SHA-256 is stored in
//! `tenant_bootstrap`) that the operator CLI prints once. Each call while bootstrap is pending
//! replaces the previous token, so only the most recently printed one works. [`complete_bootstrap`]
//! checks the token in constant time against the locked row, makes the signed-in principal the
//! tenant's `tenant_owner`, marks the row completed and appends an audit event, all in the
//! caller's transaction.
//!
//! "Forced bootstrap completion": while [`bootstrap_required`] is true the API serves only the
//! bootstrap flow. It is false once a bootstrap row is completed or the tenant already has an
//! active `tenant_owner` (a deployment that was provisioned another way).

use figcraft_cloud_domain::{PrincipalId, TenantId, Uuid};
use sqlx::PgPool;

use crate::TenantTransaction;
use crate::audit::record_audit_event;
use crate::sessions::{constant_time_eq, random_token, session_token_hash, well_formed_token};

/// Default bootstrap token lifetime (24 hours).
pub const DEFAULT_BOOTSTRAP_LIFETIME_SECS: i64 = 24 * 60 * 60;
/// Longest bootstrap token lifetime (7 days).
pub const MAX_BOOTSTRAP_LIFETIME_SECS: i64 = 7 * 24 * 60 * 60;

/// Fixed, non-leaking categories for bootstrap failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootstrapError {
    /// The lifetime is out of range.
    InvalidInput,
    /// No secure random numbers are available.
    Random,
    /// The token does not complete bootstrap (unknown, malformed, expired, superseded, or
    /// bootstrap already completed). Deliberately one category.
    Refused,
    /// The principal is not an active user.
    InvalidPrincipal,
    /// The tenant does not exist.
    Constraint,
    Database,
}

impl std::fmt::Display for BootstrapError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "bootstrap error: {self:?}")
    }
}

impl std::error::Error for BootstrapError {}

/// What [`ensure_bootstrap`] did.
#[derive(Clone, Eq, PartialEq)]
pub enum BootstrapStatus {
    /// Bootstrap is already done; no token was issued.
    Completed,
    /// A new token was issued (print it once; it is never readable again).
    Issued { token: String, expires_in_secs: i64 },
}

impl std::fmt::Debug for BootstrapStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Completed => formatter.write_str("Completed"),
            Self::Issued { expires_in_secs, .. } => {
                formatter.debug_struct("Issued").field("token", &"[REDACTED]").field("expires_in_secs", expires_in_secs).finish()
            }
        }
    }
}

fn database_error(error: &sqlx::Error) -> BootstrapError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23503" | "23514") => BootstrapError::Constraint,
        _ => BootstrapError::Database,
    }
}

const OWNED_SQL: &str = concat!(
    "SELECT EXISTS (SELECT 1 FROM tenant_bootstrap WHERE tenant_id = $1::uuid AND completed_at IS NOT NULL) ",
    "OR EXISTS (SELECT 1 FROM tenant_members m JOIN principals p ON p.id = m.principal_id ",
    "WHERE m.tenant_id = $1::uuid AND m.role = 'tenant_owner' AND m.state = 'active' AND p.state = 'active')"
);

async fn already_owned(transaction: &mut TenantTransaction<'_>) -> Result<bool, BootstrapError> {
    sqlx::query_scalar(OWNED_SQL)
        .bind(transaction.tenant_id().to_string())
        .fetch_one(transaction.connection())
        .await
        .map_err(|error| database_error(&error))
}

/// Issue (or reissue) the transaction tenant's bootstrap token valid for `lifetime_secs`
/// (1..=[`MAX_BOOTSTRAP_LIFETIME_SECS`]), unless bootstrap is already done. The caller commits.
pub async fn ensure_bootstrap(transaction: &mut TenantTransaction<'_>, lifetime_secs: i64) -> Result<BootstrapStatus, BootstrapError> {
    if !(1..=MAX_BOOTSTRAP_LIFETIME_SECS).contains(&lifetime_secs) {
        return Err(BootstrapError::InvalidInput);
    }
    // Serialize concurrent issuers for this tenant.
    let locked: Option<i32> = sqlx::query_scalar("SELECT 1 FROM tenants WHERE id = $1::uuid FOR UPDATE")
        .bind(transaction.tenant_id().to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    locked.ok_or(BootstrapError::Constraint)?;
    if already_owned(transaction).await? {
        return Ok(BootstrapStatus::Completed);
    }
    let token = random_token().ok_or(BootstrapError::Random)?;
    sqlx::query(concat!(
        "INSERT INTO tenant_bootstrap (tenant_id, token_hash, expires_at) ",
        "VALUES ($1::uuid, $2, now() + make_interval(secs => $3::bigint)) ",
        "ON CONFLICT (tenant_id) DO UPDATE SET token_hash = EXCLUDED.token_hash, created_at = now(), ",
        "expires_at = EXCLUDED.expires_at WHERE tenant_bootstrap.completed_at IS NULL"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(session_token_hash(&token).to_vec())
    .bind(lifetime_secs)
    .execute(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    Ok(BootstrapStatus::Issued { token, expires_in_secs: lifetime_secs })
}

/// Complete bootstrap with `token`: `principal_id` (an active `user` principal) becomes the
/// tenant's active `tenant_owner` and the row is marked completed by it. The caller commits, then
/// should rotate the principal's session (a privilege change).
pub async fn complete_bootstrap(transaction: &mut TenantTransaction<'_>, token: &str, principal_id: PrincipalId) -> Result<(), BootstrapError> {
    if !well_formed_token(token) {
        return Err(BootstrapError::Refused);
    }
    let tenant = transaction.tenant_id().to_string();
    let row: Option<(Vec<u8>, bool)> = sqlx::query_as(concat!(
        "SELECT token_hash, expires_at > now() FROM tenant_bootstrap ",
        "WHERE tenant_id = $1::uuid AND completed_at IS NULL FOR UPDATE"
    ))
    .bind(&tenant)
    .fetch_optional(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    let (stored, live) = row.ok_or(BootstrapError::Refused)?;
    let matches = constant_time_eq(&stored, &session_token_hash(token));
    if !(matches && live) {
        return Err(BootstrapError::Refused);
    }
    let active_user: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM principals WHERE id = $1::uuid AND kind = 'user' AND state = 'active')")
        .bind(principal_id.to_string())
        .fetch_one(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    if !active_user {
        return Err(BootstrapError::InvalidPrincipal);
    }
    sqlx::query(concat!(
        "INSERT INTO tenant_members (tenant_id, principal_id, role, state) VALUES ($1::uuid, $2::uuid, 'tenant_owner', 'active') ",
        "ON CONFLICT (tenant_id, principal_id) DO UPDATE SET role = 'tenant_owner', state = 'active', updated_at = now()"
    ))
    .bind(&tenant)
    .bind(principal_id.to_string())
    .execute(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    sqlx::query("UPDATE tenant_bootstrap SET completed_at = now(), completed_by = $2::uuid WHERE tenant_id = $1::uuid")
        .bind(&tenant)
        .bind(principal_id.to_string())
        .execute(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    record_audit_event(
        transaction,
        Uuid::new_v4(),
        Some(principal_id.as_uuid()),
        "tenant.bootstrap.complete",
        "tenant",
        Some(transaction.tenant_id().as_uuid()),
        None,
        None,
        "succeeded",
        &serde_json::json!({}),
    )
    .await
    .map_err(|_| BootstrapError::Database)?;
    Ok(())
}

/// True while `tenant_id` still needs bootstrap: no completed bootstrap row and no active
/// `tenant_owner`. Read through the pool so request middleware can call it cheaply.
pub async fn bootstrap_required(pool: &PgPool, tenant_id: TenantId) -> Result<bool, BootstrapError> {
    let done: bool = sqlx::query_scalar(OWNED_SQL).bind(tenant_id.to_string()).fetch_one(pool).await.map_err(|_| BootstrapError::Database)?;
    Ok(!done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issued_status_never_prints_its_token() {
        let status = BootstrapStatus::Issued { token: "top-secret".into(), expires_in_secs: 5 };
        assert!(!format!("{status:?}").contains("top-secret"));
    }
}

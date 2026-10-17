//! Service accounts with scoped, expiring and rotatable credentials (S4-004).
//!
//! A service account is a `service` principal plus a `service_accounts` row scoped to one tenant,
//! holding a non-empty set of scopes from the fixed allowlist [`Scope`]. Credentials are
//! `fcsa_` followed by 43 URL-safe base64 characters (32 random bytes); only the SHA-256 of the
//! whole secret is stored (unique index), it is returned once, and it expires within
//! [`MAX_CREDENTIAL_LIFETIME_SECS`]. An account holds at most [`MAX_ACTIVE_CREDENTIALS`] live
//! credentials.
//!
//! [`rotate_credential`] issues a replacement and shortens the old credential's expiry to the
//! requested overlap (at most [`MAX_ROTATION_OVERLAP_SECS`]) so deployed clients can switch over.
//! [`authenticate_credential`] refuses, identically, anything unknown, malformed, revoked,
//! expired, or whose account, principal or tenant is not active; it updates `last_used_at` at most
//! once per [`LAST_USED_INTERVAL_SECS`]. Service principals get no tenant membership: their scopes
//! are their whole authority, so interactive owner actions are never implied.

use figcraft_cloud_domain::{PrincipalId, TenantId, Uuid};
use sqlx::PgPool;

use crate::audit::record_audit_event;
use crate::sessions::{random_token, session_token_hash, well_formed_token};
use crate::{TenantTransaction, begin_tenant_transaction};

/// Prefix of every credential secret (lets secret scanners recognise leaks).
pub const SECRET_PREFIX: &str = "fcsa_";
/// Longest credential lifetime (one year).
pub const MAX_CREDENTIAL_LIFETIME_SECS: i64 = 365 * 24 * 60 * 60;
/// Most live (unrevoked, unexpired) credentials per account.
pub const MAX_ACTIVE_CREDENTIALS: i64 = 5;
/// Longest overlap during which a rotated-out credential keeps working (7 days).
pub const MAX_ROTATION_OVERLAP_SECS: i64 = 7 * 24 * 60 * 60;
/// Minimum seconds between `last_used_at` writes for one credential.
pub const LAST_USED_INTERVAL_SECS: i64 = 60;
/// Longest account name.
pub const MAX_NAME_CHARS: usize = 255;
/// Most scope strings accepted by [`parse_scopes`] before deduplication.
pub const MAX_SCOPE_INPUTS: usize = 64;

/// The fixed scope allowlist.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Scope {
    DocumentsRead,
    DocumentsWrite,
    ExportsCreate,
    JobsRead,
}

impl Scope {
    pub const ALL: [Self; 4] = [Self::DocumentsRead, Self::DocumentsWrite, Self::ExportsCreate, Self::JobsRead];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DocumentsRead => "documents:read",
            Self::DocumentsWrite => "documents:write",
            Self::ExportsCreate => "exports:create",
            Self::JobsRead => "jobs:read",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|scope| scope.as_str() == value)
    }
}

/// Parse requested scopes: every entry must be on the allowlist, at least one and at most
/// [`MAX_SCOPE_INPUTS`] entries; duplicates collapse. The result is sorted.
pub fn parse_scopes<S: AsRef<str>>(values: &[S]) -> Result<Vec<Scope>, ServiceAccountError> {
    if values.is_empty() || values.len() > MAX_SCOPE_INPUTS {
        return Err(ServiceAccountError::InvalidInput);
    }
    let mut scopes = values.iter().map(|value| Scope::parse(value.as_ref())).collect::<Option<Vec<_>>>().ok_or(ServiceAccountError::InvalidInput)?;
    scopes.sort_unstable();
    scopes.dedup();
    Ok(scopes)
}

/// Fixed, non-leaking categories for service-account failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceAccountError {
    /// Name, scopes, lifetime or overlap out of range.
    InvalidInput,
    /// No secure random numbers are available.
    Random,
    /// The secret authenticates nothing. Deliberately one category.
    Refused,
    /// No such account or credential in this tenant, or it is not in a state that allows this.
    NotFound,
    /// The account already has [`MAX_ACTIVE_CREDENTIALS`] live credentials.
    TooManyCredentials,
    /// A referenced principal or tenant does not exist.
    Constraint,
    Database,
}

impl std::fmt::Display for ServiceAccountError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "service account error: {self:?}")
    }
}

impl std::error::Error for ServiceAccountError {}

fn database_error(error: &sqlx::Error) -> ServiceAccountError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23503" | "23514") => ServiceAccountError::Constraint,
        _ => ServiceAccountError::Database,
    }
}

/// A newly created account.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServiceAccount {
    pub id: Uuid,
    pub principal_id: PrincipalId,
}

/// A credential just issued: its id and secret (never readable again).
#[derive(Clone, Eq, PartialEq)]
pub struct IssuedCredential {
    pub id: Uuid,
    pub secret: String,
    pub expires_in_secs: i64,
}

impl std::fmt::Debug for IssuedCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IssuedCredential")
            .field("id", &self.id)
            .field("secret", &"[REDACTED]")
            .field("expires_in_secs", &self.expires_in_secs)
            .finish()
    }
}

/// Who a valid secret authenticates.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthenticatedServiceAccount {
    pub tenant_id: TenantId,
    pub principal_id: PrincipalId,
    pub service_account_id: Uuid,
    pub credential_id: Uuid,
    pub scopes: Vec<Scope>,
}

fn well_formed_secret(secret: &str) -> bool {
    secret.strip_prefix(SECRET_PREFIX).is_some_and(well_formed_token)
}

/// Create a service account named `name` with `scopes` in the transaction's tenant. The caller
/// authorizes `created_by` (a tenant administrator) and commits.
pub async fn create_service_account(
    transaction: &mut TenantTransaction<'_>,
    name: &str,
    scopes: &[Scope],
    created_by: PrincipalId,
) -> Result<ServiceAccount, ServiceAccountError> {
    let trimmed = name.trim();
    if trimmed.is_empty() || name.chars().count() > MAX_NAME_CHARS || name.chars().any(char::is_control) {
        return Err(ServiceAccountError::InvalidInput);
    }
    let scopes = parse_scopes(&scopes.iter().map(|scope| scope.as_str()).collect::<Vec<_>>())?;
    let scope_strings: Vec<String> = scopes.iter().map(|scope| scope.as_str().to_owned()).collect();
    let id = Uuid::new_v4();
    let principal_id = PrincipalId::random();
    sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'service', $2)")
        .bind(principal_id.to_string())
        .bind(trimmed)
        .execute(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    sqlx::query(concat!(
        "INSERT INTO service_accounts (id, tenant_id, principal_id, name, scopes, created_by) ",
        "VALUES ($1::uuid, $2::uuid, $3::uuid, $4, $5, $6::uuid)"
    ))
    .bind(id.to_string())
    .bind(transaction.tenant_id().to_string())
    .bind(principal_id.to_string())
    .bind(trimmed)
    .bind(&scope_strings)
    .bind(created_by.to_string())
    .execute(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    audit(transaction, created_by, "service_account.create", id, serde_json::json!({ "scopes": scope_strings })).await?;
    Ok(ServiceAccount { id, principal_id })
}

async fn audit(
    transaction: &mut TenantTransaction<'_>,
    actor: PrincipalId,
    action: &str,
    resource: Uuid,
    metadata: serde_json::Value,
) -> Result<(), ServiceAccountError> {
    record_audit_event(
        transaction,
        Uuid::new_v4(),
        Some(actor.as_uuid()),
        action,
        "service_account",
        Some(resource),
        None,
        None,
        "succeeded",
        &metadata,
    )
    .await
    .map(|_| ())
    .map_err(|_| ServiceAccountError::Database)
}

fn lifetime(expires_in_secs: i64) -> Result<i64, ServiceAccountError> {
    if (1..=MAX_CREDENTIAL_LIFETIME_SECS).contains(&expires_in_secs) { Ok(expires_in_secs) } else { Err(ServiceAccountError::InvalidInput) }
}

/// Lock an active account and count its live credentials other than `excluding`.
async fn lock_active_account(transaction: &mut TenantTransaction<'_>, account_id: Uuid, excluding: Option<Uuid>) -> Result<i64, ServiceAccountError> {
    let locked: Option<i32> =
        sqlx::query_scalar("SELECT 1 FROM service_accounts WHERE tenant_id = $1::uuid AND id = $2::uuid AND state = 'active' FOR UPDATE")
            .bind(transaction.tenant_id().to_string())
            .bind(account_id.to_string())
            .fetch_optional(transaction.connection())
            .await
            .map_err(|error| database_error(&error))?;
    locked.ok_or(ServiceAccountError::NotFound)?;
    sqlx::query_scalar(concat!(
        "SELECT count(*) FROM service_account_credentials WHERE tenant_id = $1::uuid AND service_account_id = $2::uuid ",
        "AND revoked_at IS NULL AND expires_at > now() AND ($3::uuid IS NULL OR id <> $3::uuid)"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(account_id.to_string())
    .bind(excluding.map(|id| id.to_string()))
    .fetch_one(transaction.connection())
    .await
    .map_err(|error| database_error(&error))
}

async fn insert_credential(
    transaction: &mut TenantTransaction<'_>,
    account_id: Uuid,
    expires_in_secs: i64,
) -> Result<IssuedCredential, ServiceAccountError> {
    let token = random_token().ok_or(ServiceAccountError::Random)?;
    let secret = format!("{SECRET_PREFIX}{token}");
    let id = Uuid::new_v4();
    sqlx::query(concat!(
        "INSERT INTO service_account_credentials (id, tenant_id, service_account_id, secret_hash, expires_at) ",
        "VALUES ($1::uuid, $2::uuid, $3::uuid, $4, now() + make_interval(secs => $5::bigint))"
    ))
    .bind(id.to_string())
    .bind(transaction.tenant_id().to_string())
    .bind(account_id.to_string())
    .bind(session_token_hash(&secret).to_vec())
    .bind(expires_in_secs)
    .execute(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    Ok(IssuedCredential { id, secret, expires_in_secs })
}

/// Issue a credential for active account `account_id`, expiring in `expires_in_secs`
/// (1..=[`MAX_CREDENTIAL_LIFETIME_SECS`]). The caller authorizes `actor` and commits.
pub async fn issue_credential(
    transaction: &mut TenantTransaction<'_>,
    account_id: Uuid,
    expires_in_secs: i64,
    actor: PrincipalId,
) -> Result<IssuedCredential, ServiceAccountError> {
    let expires_in_secs = lifetime(expires_in_secs)?;
    if lock_active_account(transaction, account_id, None).await? >= MAX_ACTIVE_CREDENTIALS {
        return Err(ServiceAccountError::TooManyCredentials);
    }
    let issued = insert_credential(transaction, account_id, expires_in_secs).await?;
    audit(transaction, actor, "service_account.credential.issue", account_id, serde_json::json!({ "credential_id": issued.id })).await?;
    Ok(issued)
}

/// Replace live credential `credential_id` of `account_id`: a new credential is issued and the old
/// one keeps working for `overlap_secs` (0..=[`MAX_ROTATION_OVERLAP_SECS`]; never longer than it
/// would have). The credential being replaced is not counted against [`MAX_ACTIVE_CREDENTIALS`],
/// so an account at the cap can still rotate (holding cap + 1 live credentials until the overlap
/// ends). The caller commits.
pub async fn rotate_credential(
    transaction: &mut TenantTransaction<'_>,
    account_id: Uuid,
    credential_id: Uuid,
    expires_in_secs: i64,
    overlap_secs: i64,
    actor: PrincipalId,
) -> Result<IssuedCredential, ServiceAccountError> {
    let expires_in_secs = lifetime(expires_in_secs)?;
    if !(0..=MAX_ROTATION_OVERLAP_SECS).contains(&overlap_secs) {
        return Err(ServiceAccountError::InvalidInput);
    }
    if lock_active_account(transaction, account_id, Some(credential_id)).await? >= MAX_ACTIVE_CREDENTIALS {
        return Err(ServiceAccountError::TooManyCredentials);
    }
    // Overlap 0 revokes outright; otherwise the expiry is pulled in (kept > created_at by the
    // greatest() guard so the table's CHECK holds for a credential created this instant).
    let shortened = sqlx::query(concat!(
        "UPDATE service_account_credentials SET ",
        "revoked_at = CASE WHEN $4::bigint = 0 THEN now() ELSE NULL END, ",
        "expires_at = CASE WHEN $4::bigint = 0 THEN expires_at ",
        "ELSE greatest(created_at + interval '1 microsecond', least(expires_at, now() + make_interval(secs => $4::bigint))) END ",
        "WHERE tenant_id = $1::uuid AND service_account_id = $2::uuid AND id = $3::uuid ",
        "AND revoked_at IS NULL AND expires_at > now()"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(account_id.to_string())
    .bind(credential_id.to_string())
    .bind(overlap_secs)
    .execute(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    if shortened.rows_affected() != 1 {
        return Err(ServiceAccountError::NotFound);
    }
    let issued = insert_credential(transaction, account_id, expires_in_secs).await?;
    audit(
        transaction,
        actor,
        "service_account.credential.rotate",
        account_id,
        serde_json::json!({ "credential_id": issued.id, "replaces": credential_id, "overlap_secs": overlap_secs }),
    )
    .await?;
    Ok(issued)
}

/// Revoke credential `credential_id` of `account_id` now: true when it was unrevoked.
pub async fn revoke_credential(
    transaction: &mut TenantTransaction<'_>,
    account_id: Uuid,
    credential_id: Uuid,
    actor: PrincipalId,
) -> Result<bool, ServiceAccountError> {
    let result = sqlx::query(concat!(
        "UPDATE service_account_credentials SET revoked_at = now() ",
        "WHERE tenant_id = $1::uuid AND service_account_id = $2::uuid AND id = $3::uuid AND revoked_at IS NULL"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(account_id.to_string())
    .bind(credential_id.to_string())
    .execute(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    let revoked = result.rows_affected() == 1;
    if revoked {
        audit(transaction, actor, "service_account.credential.revoke", account_id, serde_json::json!({ "credential_id": credential_id })).await?;
    }
    Ok(revoked)
}

/// Disable (`enabled = false`) or re-enable account `account_id`. A disabled account's
/// credentials stop authenticating at once (they are not revoked, so re-enabling restores them).
/// Returns true when the state changed.
pub async fn set_service_account_enabled(
    transaction: &mut TenantTransaction<'_>,
    account_id: Uuid,
    enabled: bool,
    actor: PrincipalId,
) -> Result<bool, ServiceAccountError> {
    let (from, to, action) =
        if enabled { ("disabled", "active", "service_account.enable") } else { ("active", "disabled", "service_account.disable") };
    let result =
        sqlx::query("UPDATE service_accounts SET state = $4, updated_at = now() WHERE tenant_id = $1::uuid AND id = $2::uuid AND state = $3")
            .bind(transaction.tenant_id().to_string())
            .bind(account_id.to_string())
            .bind(from)
            .bind(to)
            .execute(transaction.connection())
            .await
            .map_err(|error| database_error(&error))?;
    let changed = result.rows_affected() == 1;
    if changed {
        audit(transaction, actor, action, account_id, serde_json::json!({})).await?;
    }
    Ok(changed)
}

type CredentialRow = (String, String, String, String, Vec<String>, bool);

/// Authenticate a presented secret. Runs before any tenant is known (the secret names it), so it
/// reads through the pool. Stored scopes not on the current allowlist are dropped (deny by
/// default); an account left with none is refused.
pub async fn authenticate_credential(pool: &PgPool, secret: &str) -> Result<AuthenticatedServiceAccount, ServiceAccountError> {
    if !well_formed_secret(secret) {
        return Err(ServiceAccountError::Refused);
    }
    // The tenant is not known yet, so the lookup goes through the SECURITY DEFINER function
    // (S4-014), which returns only this credential's row.
    let row: Option<CredentialRow> = sqlx::query_as(
        "SELECT credential_id, tenant_id, service_account_id, principal_id, scopes, stale FROM figcraft_service_credential($1, $2::bigint)",
    )
    .bind(session_token_hash(secret).to_vec())
    .bind(LAST_USED_INTERVAL_SECS)
    .fetch_optional(pool)
    .await
    .map_err(|_| ServiceAccountError::Database)?;
    let (credential_id, tenant_id, account_id, principal_id, stored_scopes, stale) = row.ok_or(ServiceAccountError::Refused)?;
    let mut scopes: Vec<Scope> = stored_scopes.iter().filter_map(|scope| Scope::parse(scope)).collect();
    scopes.sort_unstable();
    scopes.dedup();
    if scopes.is_empty() {
        return Err(ServiceAccountError::Refused);
    }
    let tenant: TenantId = tenant_id.parse().map_err(|_| ServiceAccountError::Database)?;
    if stale {
        let mut transaction = begin_tenant_transaction(pool, tenant).await.map_err(|_| ServiceAccountError::Database)?;
        sqlx::query(concat!(
            "UPDATE service_account_credentials SET last_used_at = now() WHERE tenant_id = $1::uuid AND id = $2::uuid ",
            "AND (last_used_at IS NULL OR last_used_at <= now() - make_interval(secs => $3::bigint))"
        ))
        .bind(tenant.to_string())
        .bind(&credential_id)
        .bind(LAST_USED_INTERVAL_SECS)
        .execute(transaction.connection())
        .await
        .map_err(|_| ServiceAccountError::Database)?;
        transaction.commit().await.map_err(|_| ServiceAccountError::Database)?;
    }
    Ok(AuthenticatedServiceAccount {
        tenant_id: tenant,
        principal_id: principal_id.parse().map_err(|_| ServiceAccountError::Database)?,
        service_account_id: account_id.parse().map_err(|_| ServiceAccountError::Database)?,
        credential_id: credential_id.parse().map_err(|_| ServiceAccountError::Database)?,
        scopes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_round_trip_and_dedupe() {
        for scope in Scope::ALL {
            assert_eq!(Scope::parse(scope.as_str()), Some(scope));
        }
        assert_eq!(parse_scopes(&["jobs:read", "documents:read", "jobs:read"]), Ok(vec![Scope::DocumentsRead, Scope::JobsRead]));
    }

    #[test]
    fn junk_scopes_are_rejected() {
        let long = "x".repeat(10_000);
        let junk: [&str; 12] = [
            "",
            " ",
            "documents",
            "documents:",
            ":read",
            "DOCUMENTS:READ",
            "documents:read ",
            " documents:read",
            "documents:read\0",
            "*",
            "admin",
            &long,
        ];
        for bad in junk {
            assert_eq!(parse_scopes(&[bad]), Err(ServiceAccountError::InvalidInput), "{bad:?}");
            assert_eq!(parse_scopes(&["jobs:read", bad]), Err(ServiceAccountError::InvalidInput), "{bad:?}");
        }
        assert_eq!(parse_scopes::<&str>(&[]), Err(ServiceAccountError::InvalidInput));
        assert_eq!(parse_scopes(&vec!["jobs:read"; MAX_SCOPE_INPUTS + 1]), Err(ServiceAccountError::InvalidInput));
        assert!(parse_scopes(&vec!["jobs:read"; MAX_SCOPE_INPUTS]).is_ok());
        // Deterministic byte sweep: nothing but the allowlist parses.
        let mut seed = 0x9e37_79b9_u32;
        for _ in 0..2_000 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let len = (seed % 24) as usize;
            let candidate: String = (0..len).map(|i| char::from(b' ' + ((seed.rotate_left(i as u32) % 95) as u8))).collect();
            if Scope::parse(&candidate).is_none() {
                assert!(parse_scopes(&[candidate.as_str()]).is_err());
            }
        }
    }

    #[test]
    fn secrets_are_prefixed_and_checked() {
        let token = random_token().unwrap();
        assert!(well_formed_secret(&format!("{SECRET_PREFIX}{token}")));
        assert!(!well_formed_secret(&token));
        assert!(!well_formed_secret(&format!("fcsb_{token}")));
        assert!(!well_formed_secret(SECRET_PREFIX));
    }

    #[test]
    fn issued_credentials_never_print_their_secret() {
        let issued = IssuedCredential { id: Uuid::nil(), secret: "top-secret".into(), expires_in_secs: 1 };
        assert!(!format!("{issued:?}").contains("top-secret"));
    }
}

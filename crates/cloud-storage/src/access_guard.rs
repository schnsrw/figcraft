//! Holding the caller's access for the length of a mutating transaction (S4-023).
//!
//! A request is authenticated (session or credential resolved, tenant membership read) before
//! its handler opens the tenant transaction that performs the mutation. Without more, a
//! revocation that commits between those two points (logout, "sign out everywhere", principal
//! suspension, removal from the tenant, a revoked service-account credential or share link)
//! would not stop the mutation from committing after it.
//!
//! [`hold_caller_access`] closes that window. Called inside the mutating transaction, before
//! its authorization reads and its write, it re-reads the rows the caller's access rests on with
//! `FOR SHARE` and refuses when any of them is no longer live:
//!
//! * browser session: the `principals` row (active), then the `browser_sessions` row (not
//!   revoked, not expired), then, when the route acts as a member, the caller's
//!   `tenant_members` row (active);
//! * service account: its `principals` row (active), its `service_accounts` row (active), then
//!   the `service_account_credentials` row (not revoked, not expired);
//! * share session: its `share_links` row (not revoked, not expired).
//!
//! Every revocation writes one of those rows (`UPDATE` or `DELETE`, which takes the row's
//! exclusive lock), so the two serialise on the row: either the revocation committed first, and
//! the `FOR SHARE` read (READ COMMITTED re-checks the newest row version against the predicate)
//! finds nothing and the request is refused before writing anything; or the request holds the
//! share lock first, and the revocation waits for the request's transaction to end, so the
//! mutation commits before the revocation does. A mutation never commits after a revocation that
//! committed first.
//!
//! Lock order is principals, then the credential row, then the membership row: the same order
//! `revoke_all_for_principal` and session rotation use (principals, then sessions), so the guard
//! adds no lock-order inversion against them. Reads are not guarded (they change nothing).

use figcraft_cloud_domain::{PrincipalId, Uuid};

use crate::TenantTransaction;

/// The credential a request was authenticated with: its kind and row id (never the secret).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeldCredential {
    /// A `browser_sessions` row.
    BrowserSession(Uuid),
    /// A `service_account_credentials` row in the transaction's tenant.
    ServiceAccountCredential(Uuid),
    /// A `share_links` row in the transaction's tenant.
    ShareLink(Uuid),
}

/// Whether the caller acts as a tenant member (and so needs an active membership row).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MembershipRequirement {
    /// The route acts as a member of the transaction's tenant.
    ActiveMember,
    /// The route does not need a membership (e.g. creating a new tenant).
    None,
}

/// Why the caller's access could not be held. Fixed, non-leaking categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessGuardError {
    /// The principal, session, credential or link is no longer live.
    Revoked,
    /// The credential is live but the caller is no longer an active member of the tenant.
    NotMember,
    Database,
}

impl std::fmt::Display for AccessGuardError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "access guard error: {self:?}")
    }
}

impl std::error::Error for AccessGuardError {}

async fn exists(transaction: &mut TenantTransaction<'_>, sql: &'static str, binds: &[String]) -> Result<bool, AccessGuardError> {
    let mut query = sqlx::query_scalar::<_, i32>(sql);
    for bind in binds {
        query = query.bind(bind.as_str());
    }
    let found = query.fetch_optional(transaction.connection()).await.map_err(|_| AccessGuardError::Database)?;
    Ok(found.is_some())
}

const PRINCIPAL_SQL: &str = "SELECT 1 FROM principals WHERE id = $1::uuid AND state = 'active' FOR SHARE";
const SESSION_SQL: &str = concat!(
    "SELECT 1 FROM browser_sessions WHERE id = $1::uuid AND principal_id = $2::uuid ",
    "AND revoked_at IS NULL AND idle_expires_at > now() AND absolute_expires_at > now() FOR SHARE"
);
const MEMBER_SQL: &str = "SELECT 1 FROM tenant_members WHERE tenant_id = $1::uuid AND principal_id = $2::uuid AND state = 'active' FOR SHARE";
const SERVICE_ACCOUNT_SQL: &str = concat!(
    "SELECT 1 FROM service_accounts WHERE tenant_id = $1::uuid AND principal_id = $2::uuid AND state = 'active' ",
    "AND id = (SELECT service_account_id FROM service_account_credentials WHERE tenant_id = $1::uuid AND id = $3::uuid) FOR SHARE"
);
const SERVICE_CREDENTIAL_SQL: &str = concat!(
    "SELECT 1 FROM service_account_credentials WHERE tenant_id = $1::uuid AND id = $2::uuid ",
    "AND revoked_at IS NULL AND expires_at > now() FOR SHARE"
);
const SHARE_LINK_SQL: &str = concat!(
    "SELECT 1 FROM share_links WHERE tenant_id = $1::uuid AND id = $2::uuid ",
    "AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > now()) FOR SHARE"
);

/// Re-validate and hold `principal`'s access through `credential` until `transaction` ends. See
/// the module docs. Call it before the transaction's authorization reads and writes; on `Err` the
/// caller must not write (dropping the transaction rolls it back).
pub async fn hold_caller_access(
    transaction: &mut TenantTransaction<'_>,
    principal: PrincipalId,
    credential: HeldCredential,
    membership: MembershipRequirement,
) -> Result<(), AccessGuardError> {
    let tenant = transaction.tenant_id().to_string();
    let principal = principal.to_string();
    match credential {
        HeldCredential::BrowserSession(session) => {
            if !exists(transaction, PRINCIPAL_SQL, std::slice::from_ref(&principal)).await? {
                return Err(AccessGuardError::Revoked);
            }
            if !exists(transaction, SESSION_SQL, &[session.to_string(), principal.clone()]).await? {
                return Err(AccessGuardError::Revoked);
            }
            if membership == MembershipRequirement::ActiveMember && !exists(transaction, MEMBER_SQL, &[tenant, principal]).await? {
                return Err(AccessGuardError::NotMember);
            }
        }
        HeldCredential::ServiceAccountCredential(credential) => {
            if !exists(transaction, PRINCIPAL_SQL, std::slice::from_ref(&principal)).await?
                || !exists(transaction, SERVICE_ACCOUNT_SQL, &[tenant.clone(), principal, credential.to_string()]).await?
                || !exists(transaction, SERVICE_CREDENTIAL_SQL, &[tenant, credential.to_string()]).await?
            {
                return Err(AccessGuardError::Revoked);
            }
        }
        HeldCredential::ShareLink(link) => {
            if !exists(transaction, SHARE_LINK_SQL, &[tenant, link.to_string()]).await? {
                return Err(AccessGuardError::Revoked);
            }
        }
    }
    Ok(())
}

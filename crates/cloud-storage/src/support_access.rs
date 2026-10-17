//! Support-access grants (S4-018): time-boxed, audited operator access to one tenant.
//!
//! A tenant member holding `support.grant` (a tenant admin with the explicit `support`
//! permission; the API authorizes that with `figcraft_cloud_auth::authz::decide`) lets one
//! operator principal into the tenant for a stated reason, for at most [`MAX_GRANT_SECS`] (7 days,
//! also enforced by the table's CHECK), with scope [`SupportScope::Read`] or
//! [`SupportScope::Write`]. A grant is *live* while it is neither revoked nor expired.
//!
//! Every grant, revoke and use writes an audit event in the same transaction, carrying the reason:
//!
//! | event | action | actor | metadata |
//! |---|---|---|---|
//! | grant | `support.grant` | the granting member | `grant`, `granted_to`, `scope`, `expires_at`, `reason` |
//! | revoke | `support.revoke` | whoever revoked | `grant`, `granted_to`, `reason` (why it was revoked), `grant_reason` |
//! | use | `support.use` | the operator | `grant`, `scope`, `reason` (the grant's), `action`, `resource_kind` |
//!
//! # How authorization uses a grant
//!
//! `authz::Subject` describes tenant *membership*; an operator is not a member. The authenticator
//! calls [`use_support_access`] when a principal without an active membership acts in a tenant;
//! on `Some(access)` it builds the subject with `tenant_role = Some(access.tenant_role())` and no
//! sensitive permissions, and uses [`SupportAccess::resource_role`] as the effective role on every
//! workspace, project and document. A read grant therefore acts as a tenant `viewer` who can view
//! every resource; a write grant as an `editor` on every resource. Neither ever administers the
//! tenant, its members, billing or keys (those need tenant roles and sensitive permissions a
//! grant never carries), and a suspended tenant still denies everything (`decide` checks the
//! tenant state first). [`active_support_access`] answers the same question without auditing, for
//! display only: anything that *acts* on the grant must call [`use_support_access`].
//!
//! Operator principals are ordinary principals: the deployment decides who may be granted (the
//! operator console); this module only requires the grantee to be an active principal other than
//! the granter.

use figcraft_cloud_domain::{PrincipalId, Role, Uuid};

use crate::TenantTransaction;
use crate::audit::record_audit_event;

/// Longest a grant may last (7 days; the table's CHECK enforces the same bound).
pub const MAX_GRANT_SECS: u64 = 7 * 24 * 60 * 60;
/// Longest reason accepted, in characters after trimming (the table's CHECK is the same).
pub const MAX_REASON_CHARS: usize = 500;
/// Most grants [`list_active_support_grants`] returns (soonest expiry first).
pub const MAX_LISTED_GRANTS: i64 = 200;
/// Longest action / resource-kind label recorded for a use.
const MAX_LABEL_CHARS: usize = 128;

/// What a grant lets the operator do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupportScope {
    Read,
    Write,
}

impl SupportScope {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            _ => None,
        }
    }
}

/// Fixed, non-leaking categories for support-access failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupportAccessError {
    /// The reason is empty, longer than [`MAX_REASON_CHARS`] or holds control characters.
    InvalidReason,
    /// The lifetime is zero or longer than [`MAX_GRANT_SECS`].
    InvalidExpiry,
    /// The action or resource-kind label for a use is unusable.
    InvalidLabel,
    /// The grantee is the granter, or not an active principal.
    InvalidGrantee,
    /// No such live grant in this tenant.
    NotFound,
    Audit,
    Database,
    CorruptRow,
}

impl std::fmt::Display for SupportAccessError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "support access error: {self:?}")
    }
}

impl std::error::Error for SupportAccessError {}

/// A grant row. Times are Unix seconds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SupportGrant {
    pub id: Uuid,
    pub granted_to: PrincipalId,
    pub granted_by: PrincipalId,
    pub reason: String,
    pub scope: SupportScope,
    pub created_at: i64,
    pub expires_at: i64,
    pub revoked_at: Option<i64>,
}

/// The live grant a principal acts under.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SupportAccess {
    pub grant_id: Uuid,
    pub scope: SupportScope,
    pub reason: String,
    pub expires_at: i64,
}

impl SupportAccess {
    /// The tenant role the authenticator gives the operator's `Subject` (never an admin role).
    #[must_use]
    pub const fn tenant_role(&self) -> Role {
        match self.scope {
            SupportScope::Read => Role::Viewer,
            SupportScope::Write => Role::Editor,
        }
    }

    /// The effective role on every workspace, project and document of the tenant.
    #[must_use]
    pub const fn resource_role(&self) -> Role {
        self.tenant_role()
    }
}

/// The trimmed reason, or `None` when it is empty, too long or holds control characters.
#[must_use]
pub fn clean_reason(reason: &str) -> Option<&str> {
    let reason = reason.trim();
    let chars = reason.chars().count();
    ((1..=MAX_REASON_CHARS).contains(&chars) && !reason.chars().any(char::is_control)).then_some(reason)
}

fn clean_label(label: &str) -> Option<&str> {
    let chars = label.chars().count();
    ((1..=MAX_LABEL_CHARS).contains(&chars) && label.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')))
        .then_some(label)
}

type GrantRow = (String, String, String, String, String, i64, i64, Option<i64>);

macro_rules! grant_columns {
    () => {
        concat!(
            "id::text, granted_to::text, granted_by::text, reason, scope, floor(extract(epoch FROM created_at))::bigint, ",
            "floor(extract(epoch FROM expires_at))::bigint, floor(extract(epoch FROM revoked_at))::bigint"
        )
    };
}

fn grant(row: GrantRow) -> Result<SupportGrant, SupportAccessError> {
    let (id, granted_to, granted_by, reason, scope, created_at, expires_at, revoked_at) = row;
    Ok(SupportGrant {
        id: id.parse().map_err(|_| SupportAccessError::CorruptRow)?,
        granted_to: granted_to.parse().map_err(|_| SupportAccessError::CorruptRow)?,
        granted_by: granted_by.parse().map_err(|_| SupportAccessError::CorruptRow)?,
        reason,
        scope: SupportScope::parse(&scope).ok_or(SupportAccessError::CorruptRow)?,
        created_at,
        expires_at,
        revoked_at,
    })
}

async fn audit(
    transaction: &mut TenantTransaction<'_>,
    actor: PrincipalId,
    action: &str,
    grant_id: Uuid,
    metadata: &serde_json::Value,
) -> Result<(), SupportAccessError> {
    record_audit_event(transaction, Uuid::new_v4(), Some(actor.as_uuid()), action, "support_grant", Some(grant_id), None, None, "succeeded", metadata)
        .await
        .map(|_| ())
        .map_err(|_| SupportAccessError::Audit)
}

/// Grant `granted_to` support access to the transaction's tenant for `reason`, expiring
/// `expires_in_secs` from now (1..=[`MAX_GRANT_SECS`]). The caller must have authorized
/// `granted_by` for `support.grant`; the caller commits. Records `support.grant`.
pub async fn grant_support_access(
    transaction: &mut TenantTransaction<'_>,
    granted_to: PrincipalId,
    granted_by: PrincipalId,
    reason: &str,
    scope: SupportScope,
    expires_in_secs: u64,
) -> Result<SupportGrant, SupportAccessError> {
    let reason = clean_reason(reason).ok_or(SupportAccessError::InvalidReason)?;
    if expires_in_secs == 0 || expires_in_secs > MAX_GRANT_SECS {
        return Err(SupportAccessError::InvalidExpiry);
    }
    let lifetime = i64::try_from(expires_in_secs).map_err(|_| SupportAccessError::InvalidExpiry)?;
    if granted_to == granted_by {
        return Err(SupportAccessError::InvalidGrantee);
    }
    let id = Uuid::new_v4();
    let row: Option<GrantRow> = sqlx::query_as(concat!(
        "INSERT INTO support_access_grants (id, tenant_id, granted_to, granted_by, reason, scope, expires_at) ",
        "SELECT $1::uuid, $2::uuid, p.id, $4::uuid, $5, $6, now() + make_interval(secs => $7::bigint) ",
        "FROM principals p WHERE p.id = $3::uuid AND p.state = 'active' RETURNING ",
        grant_columns!()
    ))
    .bind(id.to_string())
    .bind(transaction.tenant_id().to_string())
    .bind(granted_to.to_string())
    .bind(granted_by.to_string())
    .bind(reason)
    .bind(scope.as_str())
    .bind(lifetime)
    .fetch_optional(transaction.connection())
    .await
    .map_err(|error| match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23503") => SupportAccessError::InvalidGrantee,
        Some("23514") => SupportAccessError::InvalidExpiry,
        _ => SupportAccessError::Database,
    })?;
    let record = grant(row.ok_or(SupportAccessError::InvalidGrantee)?)?;
    let metadata = serde_json::json!({
        "grant": record.id.to_string(),
        "granted_to": record.granted_to.to_string(),
        "scope": record.scope.as_str(),
        "expires_at": record.expires_at,
        "reason": record.reason,
    });
    audit(transaction, granted_by, "support.grant", record.id, &metadata).await?;
    Ok(record)
}

/// Revoke live grant `grant_id` in the transaction's tenant, recording `support.revoke` with why
/// (`reason`, same bounds as a grant's) and the grant's own reason. `NotFound` when there is no
/// such live grant. The caller commits.
pub async fn revoke_support_access(
    transaction: &mut TenantTransaction<'_>,
    grant_id: Uuid,
    actor: PrincipalId,
    reason: &str,
) -> Result<SupportGrant, SupportAccessError> {
    let reason = clean_reason(reason).ok_or(SupportAccessError::InvalidReason)?;
    let row: Option<GrantRow> = sqlx::query_as(concat!(
        "UPDATE support_access_grants SET revoked_at = now() ",
        "WHERE tenant_id = $1::uuid AND id = $2::uuid AND revoked_at IS NULL AND expires_at > now() RETURNING ",
        grant_columns!()
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(grant_id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(|_| SupportAccessError::Database)?;
    let record = grant(row.ok_or(SupportAccessError::NotFound)?)?;
    let metadata = serde_json::json!({
        "grant": record.id.to_string(),
        "granted_to": record.granted_to.to_string(),
        "reason": reason,
        "grant_reason": record.reason,
    });
    audit(transaction, actor, "support.revoke", record.id, &metadata).await?;
    Ok(record)
}

/// The tenant's live grants, soonest expiry first, at most [`MAX_LISTED_GRANTS`].
pub async fn list_active_support_grants(transaction: &mut TenantTransaction<'_>) -> Result<Vec<SupportGrant>, SupportAccessError> {
    let rows: Vec<GrantRow> = sqlx::query_as(concat!(
        "SELECT ",
        grant_columns!(),
        " FROM support_access_grants WHERE tenant_id = $1::uuid AND revoked_at IS NULL AND expires_at > now() ",
        "ORDER BY expires_at, id LIMIT $2"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(MAX_LISTED_GRANTS)
    .fetch_all(transaction.connection())
    .await
    .map_err(|_| SupportAccessError::Database)?;
    rows.into_iter().map(grant).collect()
}

/// The live grant `principal` holds in the transaction's tenant (the widest scope, then the
/// latest expiry), without auditing. For display only.
pub async fn active_support_access(
    transaction: &mut TenantTransaction<'_>,
    principal: PrincipalId,
) -> Result<Option<SupportAccess>, SupportAccessError> {
    let row: Option<(String, String, String, i64)> = sqlx::query_as(concat!(
        "SELECT g.id::text, g.scope, g.reason, floor(extract(epoch FROM g.expires_at))::bigint ",
        "FROM support_access_grants g JOIN principals p ON p.id = g.granted_to ",
        "WHERE g.tenant_id = $1::uuid AND g.granted_to = $2::uuid AND g.revoked_at IS NULL AND g.expires_at > now() ",
        "AND p.state = 'active' ORDER BY (g.scope = 'write') DESC, g.expires_at DESC, g.id LIMIT 1"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(principal.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(|_| SupportAccessError::Database)?;
    row.map(|(id, scope, reason, expires_at)| {
        Ok(SupportAccess {
            grant_id: id.parse().map_err(|_| SupportAccessError::CorruptRow)?,
            scope: SupportScope::parse(&scope).ok_or(SupportAccessError::CorruptRow)?,
            reason,
            expires_at,
        })
    })
    .transpose()
}

/// Check that `principal` may act in the transaction's tenant under a live support grant and, if
/// so, record `support.use` (with the grant's reason, the `action` id and the `resource_kind`,
/// both lowercase dotted labels) in the same transaction. `None`, with nothing recorded, when no
/// live grant exists. A [`SupportScope::Read`] grant asked for a write still returns it: the
/// caller's `decide` refuses the write by role. The caller commits (an access that is refused
/// later in the request still leaves the use recorded only if the caller commits).
pub async fn use_support_access(
    transaction: &mut TenantTransaction<'_>,
    principal: PrincipalId,
    action: &str,
    resource_kind: &str,
) -> Result<Option<SupportAccess>, SupportAccessError> {
    let (Some(action), Some(resource_kind)) = (clean_label(action), clean_label(resource_kind)) else {
        return Err(SupportAccessError::InvalidLabel);
    };
    let Some(access) = active_support_access(transaction, principal).await? else {
        return Ok(None);
    };
    let metadata = serde_json::json!({
        "grant": access.grant_id.to_string(),
        "scope": access.scope.as_str(),
        "reason": access.reason,
        "action": action,
        "resource_kind": resource_kind,
    });
    audit(transaction, principal, "support.use", access.grant_id, &metadata).await?;
    Ok(Some(access))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_are_trimmed_bounded_and_printable() {
        assert_eq!(clean_reason("  ticket 42  "), Some("ticket 42"));
        assert_eq!(clean_reason("   "), None);
        assert_eq!(clean_reason(""), None);
        assert_eq!(clean_reason("a\u{7}b"), None);
        assert_eq!(clean_reason(&"é".repeat(500)).map(|r| r.chars().count()), Some(500));
        assert_eq!(clean_reason(&"é".repeat(501)), None);
    }

    #[test]
    fn scopes_round_trip_and_map_to_non_admin_roles() {
        for scope in [SupportScope::Read, SupportScope::Write] {
            assert_eq!(SupportScope::parse(scope.as_str()), Some(scope));
        }
        assert_eq!(SupportScope::parse("admin"), None);
        let access = |scope| SupportAccess { grant_id: Uuid::nil(), scope, reason: "r".into(), expires_at: 0 };
        assert_eq!(access(SupportScope::Read).tenant_role(), Role::Viewer);
        assert_eq!(access(SupportScope::Write).resource_role(), Role::Editor);
    }

    #[test]
    fn labels_are_lowercase_dotted() {
        assert_eq!(clean_label("document.view"), Some("document.view"));
        assert_eq!(clean_label("Document.View"), None);
        assert_eq!(clean_label(""), None);
        assert_eq!(clean_label(&"a".repeat(129)), None);
    }
}

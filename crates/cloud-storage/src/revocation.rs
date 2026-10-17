//! Closing access after a revocation (S4-017).
//!
//! # Policy
//!
//! Browser sessions belong to a *principal*, not to a tenant: one session can open every tenant
//! the principal belongs to. Tenant scope is therefore never cached in a session. Every API
//! request re-resolves the principal's tenant access ([`authorize_session_in_tenant`] is the
//! check; the authenticator calls it, or does the same with `resolve_session` and
//! `resolve_tenant_access`), and every relay connection admitted as a member is re-checked by the
//! relay's revocation sweep ([`dead_member_admissions`]). So a membership, team membership, team
//! grant or tenant revocation takes effect:
//!
//! * on the **next API request** of the affected principal (no cache to invalidate), and
//! * on **live relay connections** within one sweep interval (5 s by default) plus one database
//!   round trip: the sweep finds the admission dead and the hub disconnects it.
//!
//! On top of that, [`apply_revocation`] (which the API calls after committing the mutation)
//! revokes **all sessions** of a principal who has just lost their *last* active membership in an
//! active tenant (reason `membership_revoked`), or who was suspended (reason
//! `principal_suspended`), so a principal with nowhere left to go is signed out everywhere. A
//! principal removed from one tenant but still a member elsewhere keeps their sessions: revoking
//! them would sign them out of tenants that still trust them, and the per-request check already
//! refuses the tenant they lost. A per-tenant revocation epoch was considered and rejected: it
//! needs a schema change for no gain while tenant access is re-resolved on every request.
//!
//! # Members in the relay
//!
//! The relay admits share-link holders (anonymous) and, with a hello token of the form
//! `member.<tenant uuid>.<session token>` ([`MEMBER_TOKEN_PREFIX`], [`parse_member_token`]),
//! signed-in members: [`admit_member`] resolves the session, then the member's effective role on
//! the document. `document.join_realtime` needs an active document and at least `viewer`; editing
//! needs at least `editor` (the authz matrix). The relay records the admitted principal, session
//! and access; [`dead_member_admissions`] reports an admission dead when the session is no longer
//! live, the principal lost access to the document (any source), or an editor was downgraded
//! below `editor` (they reconnect with view access).
//!
//! Tenant reads here go through tenant transactions (RLS-safe); only session liveness reads
//! `browser_sessions`, which is per principal.

use std::collections::{BTreeMap, HashSet};

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, ResourceState, TenantId, Uuid};
use sqlx::PgPool;

use crate::begin_tenant_transaction;
use crate::effective_roles::{ResourceAccess, TenantAccess, resolve_document_access, resolve_tenant_access, resource_rank};
use crate::metadata::PageSize;
use crate::sessions::{ResolvedSession, RevokeReason, SessionError, SessionLimits, resolve_session, revoke_all_for_principal};
use crate::tenants::list_tenants_for_principal;

/// The prefix of a member's relay hello token.
pub const MEMBER_TOKEN_PREFIX: &str = "member.";
/// Most member admissions one [`dead_member_admissions`] call checks.
pub const MAX_CHECKED_MEMBERS: usize = 1024;
/// Longest member token accepted (prefix, tenant UUID, dot, 43-character session token).
const MAX_MEMBER_TOKEN_CHARS: usize = 128;

/// Fixed, non-leaking categories for revocation helpers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RevocationError {
    /// The credential opens nothing in this tenant (any reason: read alike).
    Refused,
    /// Too many admissions were passed in one call.
    InvalidInput,
    Database,
}

impl std::fmt::Display for RevocationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "revocation error: {self:?}")
    }
}

impl std::error::Error for RevocationError {}

/// A committed change that removed or reduced someone's access in one tenant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessRevocation {
    /// `remove_tenant_member`.
    TenantMemberRemoved { principal: PrincipalId },
    /// `suspend_tenant_member`.
    TenantMemberSuspended { principal: PrincipalId },
    /// The principal itself was suspended or deleted.
    PrincipalSuspended { principal: PrincipalId },
    /// `remove_team_member` (or a team-role downgrade).
    TeamMemberRemoved { team: Uuid, principal: PrincipalId },
    /// A team was archived.
    TeamArchived { team: Uuid },
    /// A direct workspace/document membership or a workspace/document team grant was removed or
    /// downgraded.
    GrantRevoked,
    /// The tenant was suspended.
    TenantSuspended,
}

/// What [`apply_revocation`] did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RevocationOutcome {
    /// Browser sessions revoked now.
    pub sessions_revoked: u64,
}

/// Whether `principal` still holds an active membership in some active tenant. A principal with
/// more memberships than one page lists is treated as still a member (never signs anyone out by
/// mistake).
async fn has_active_tenant(pool: &PgPool, principal: PrincipalId) -> Result<bool, RevocationError> {
    let size = PageSize::new(PageSize::MAX).map_err(|_| RevocationError::Database)?;
    let page = list_tenants_for_principal(pool, principal, None, size).await.map_err(|_| RevocationError::Database)?;
    Ok(page.next_cursor.is_some() || page.items.iter().any(|membership| membership.tenant.state == ResourceState::Active))
}

async fn revoke_all(pool: &PgPool, principal: PrincipalId, reason: RevokeReason) -> Result<u64, RevocationError> {
    let mut total = 0u64;
    // Each call revokes at most MAX_REVOKE_BATCH; a handful of rounds covers any real principal.
    for _ in 0..16 {
        let revoked = revoke_all_for_principal(pool, principal, reason).await.map_err(|_| RevocationError::Database)?;
        total = total.saturating_add(revoked);
        if revoked < u64::try_from(crate::sessions::MAX_REVOKE_BATCH).unwrap_or(u64::MAX) {
            break;
        }
    }
    Ok(total)
}

/// Apply the session side of `revocation` after the mutation committed (see the module docs).
/// Team, grant and tenant-suspension revocations change no session: the next API request and the
/// relay's next sweep re-resolve access.
pub async fn apply_revocation(pool: &PgPool, revocation: AccessRevocation) -> Result<RevocationOutcome, RevocationError> {
    let sessions_revoked = match revocation {
        AccessRevocation::TenantMemberRemoved { principal } | AccessRevocation::TenantMemberSuspended { principal } => {
            if has_active_tenant(pool, principal).await? { 0 } else { revoke_all(pool, principal, RevokeReason::MembershipRevoked).await? }
        }
        AccessRevocation::PrincipalSuspended { principal } => revoke_all(pool, principal, RevokeReason::PrincipalSuspended).await?,
        AccessRevocation::TeamMemberRemoved { .. }
        | AccessRevocation::TeamArchived { .. }
        | AccessRevocation::GrantRevoked
        | AccessRevocation::TenantSuspended => 0,
    };
    Ok(RevocationOutcome { sessions_revoked })
}

/// The per-request check: `token` must be a live session whose principal is an active member of
/// active `tenant`. Everything else is [`RevocationError::Refused`].
pub async fn authorize_session_in_tenant(
    pool: &PgPool,
    token: &str,
    limits: SessionLimits,
    tenant: TenantId,
) -> Result<(ResolvedSession, TenantAccess), RevocationError> {
    let session = resolve_session(pool, token, limits).await.map_err(|error| match error {
        SessionError::Database => RevocationError::Database,
        _ => RevocationError::Refused,
    })?;
    if session.embed.as_ref().is_some_and(|embed| embed.tenant_id != tenant) {
        return Err(RevocationError::Refused);
    }
    let mut transaction = begin_tenant_transaction(pool, tenant).await.map_err(|_| RevocationError::Database)?;
    let access = resolve_tenant_access(&mut transaction, session.principal_id).await.map_err(|_| RevocationError::Database)?;
    transaction.rollback().await.map_err(|_| RevocationError::Database)?;
    match access {
        Some(access) if access.principal_active && access.tenant_state == ResourceState::Active && access.tenant_role.is_some() => {
            Ok((session, access))
        }
        _ => Err(RevocationError::Refused),
    }
}

/// Split a member hello token into its tenant and session token; `None` for anything else
/// (share-link secrets never start with the prefix's `.`-containing form).
#[must_use]
pub fn parse_member_token(token: &str) -> Option<(TenantId, &str)> {
    if token.len() > MAX_MEMBER_TOKEN_CHARS {
        return None;
    }
    let rest = token.strip_prefix(MEMBER_TOKEN_PREFIX)?;
    let (tenant, session) = rest.split_once('.')?;
    Some((tenant.parse().ok()?, session))
}

/// The realtime access resolved `access` gives: `Some(true)` edit, `Some(false)` view, `None`
/// nothing (`document.join_realtime` needs an active document and at least `viewer`;
/// `document.edit` needs `editor`).
#[must_use]
pub fn realtime_access(access: &ResourceAccess) -> Option<bool> {
    let tenant_ok = access.tenant.principal_active && access.tenant.tenant_state == ResourceState::Active && access.tenant.tenant_role.is_some();
    let rank = access.role.map_or(0, resource_rank);
    (tenant_ok && access.state == ResourceState::Active && rank >= 1).then_some(rank >= 3)
}

/// A member the relay admitted (what it watches).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MemberAdmission {
    pub tenant: TenantId,
    pub document: CloudDocumentId,
    pub principal: PrincipalId,
    pub session: Uuid,
    /// Admitted with edit access.
    pub edit: bool,
}

/// Admit a member to `document` with a session `token` in `tenant`. Refusals read alike.
pub async fn admit_member(
    pool: &PgPool,
    tenant: TenantId,
    token: &str,
    document: CloudDocumentId,
    limits: SessionLimits,
) -> Result<MemberAdmission, RevocationError> {
    let session = resolve_session(pool, token, limits).await.map_err(|error| match error {
        SessionError::Database => RevocationError::Database,
        _ => RevocationError::Refused,
    })?;
    let mut transaction = begin_tenant_transaction(pool, tenant).await.map_err(|_| RevocationError::Database)?;
    let access = resolve_document_access(&mut transaction, session.principal_id, document).await.map_err(|_| RevocationError::Database)?;
    transaction.rollback().await.map_err(|_| RevocationError::Database)?;
    let mut edit = access.as_ref().and_then(realtime_access).ok_or(RevocationError::Refused)?;
    // An embedded session reaches only the document and role its host's token granted (S6-010).
    if let Some(embed) = &session.embed {
        if embed.tenant_id != tenant || embed.document_id.to_string() != document.to_string() {
            return Err(RevocationError::Refused);
        }
        edit = edit && embed.role == crate::host_systems::HostRole::Editor;
    }
    Ok(MemberAdmission { tenant, document, principal: session.principal_id, session: session.id, edit })
}

/// Of `admissions` (at most [`MAX_CHECKED_MEMBERS`]), those that no longer hold: the session is
/// revoked or expired, the principal is no longer allowed to join the document, or an edit
/// admission no longer has edit access.
pub async fn dead_member_admissions(pool: &PgPool, admissions: &[MemberAdmission]) -> Result<Vec<MemberAdmission>, RevocationError> {
    if admissions.is_empty() {
        return Ok(Vec::new());
    }
    if admissions.len() > MAX_CHECKED_MEMBERS {
        return Err(RevocationError::InvalidInput);
    }
    let ids: Vec<String> = admissions.iter().map(|admission| admission.session.to_string()).collect();
    let live: Vec<String> = sqlx::query_scalar(concat!(
        "SELECT s.id::text FROM browser_sessions s JOIN principals p ON p.id = s.principal_id ",
        "WHERE s.id = ANY($1::text[]::uuid[]) AND s.revoked_at IS NULL AND s.idle_expires_at > now() ",
        "AND s.absolute_expires_at > now() AND p.state = 'active'"
    ))
    .bind(ids)
    .fetch_all(pool)
    .await
    .map_err(|_| RevocationError::Database)?;
    let live: HashSet<Uuid> = live.iter().filter_map(|id| id.parse().ok()).collect();

    let mut dead = Vec::new();
    let mut by_tenant: BTreeMap<TenantId, Vec<MemberAdmission>> = BTreeMap::new();
    for admission in admissions {
        if live.contains(&admission.session) {
            by_tenant.entry(admission.tenant).or_default().push(*admission);
        } else {
            dead.push(*admission);
        }
    }
    for (tenant, admissions) in by_tenant {
        let mut transaction = begin_tenant_transaction(pool, tenant).await.map_err(|_| RevocationError::Database)?;
        for admission in admissions {
            let access =
                resolve_document_access(&mut transaction, admission.principal, admission.document).await.map_err(|_| RevocationError::Database)?;
            let holds = match access.as_ref().and_then(realtime_access) {
                Some(edit) => edit || !admission.edit,
                None => false,
            };
            if !holds {
                dead.push(admission);
            }
        }
        transaction.rollback().await.map_err(|_| RevocationError::Database)?;
    }
    Ok(dead)
}

#[cfg(test)]
mod tests {
    use super::*;
    use figcraft_cloud_domain::Role;

    #[test]
    fn member_tokens_name_a_tenant_and_a_session() {
        let tenant = TenantId::random();
        let token = format!("member.{tenant}.{}", "a".repeat(43));
        assert_eq!(parse_member_token(&token), Some((tenant, "a".repeat(43).as_str())));
        assert_eq!(parse_member_token(&"a".repeat(43)), None);
        assert_eq!(parse_member_token("member.not-a-uuid.abc"), None);
        assert_eq!(parse_member_token(&format!("member.{tenant}")), None);
        assert_eq!(parse_member_token(&format!("member.{tenant}.{}", "a".repeat(200))), None);
    }

    #[test]
    fn realtime_access_follows_the_matrix() {
        let access = |state, role| ResourceAccess {
            tenant: TenantAccess {
                principal_active: true,
                tenant_state: ResourceState::Active,
                tenant_role: Some(Role::Editor),
                permissions: Vec::new(),
            },
            state,
            role,
            sources: Vec::new(),
        };
        assert_eq!(realtime_access(&access(ResourceState::Active, Some(Role::Editor))), Some(true));
        assert_eq!(realtime_access(&access(ResourceState::Active, Some(Role::Commenter))), Some(false));
        assert_eq!(realtime_access(&access(ResourceState::Active, Some(Role::Viewer))), Some(false));
        assert_eq!(realtime_access(&access(ResourceState::Active, None)), None);
        assert_eq!(realtime_access(&access(ResourceState::Archived, Some(Role::WorkspaceAdmin))), None);
        let mut removed = access(ResourceState::Active, Some(Role::Editor));
        removed.tenant.tenant_role = None;
        assert_eq!(realtime_access(&removed), None);
    }
}

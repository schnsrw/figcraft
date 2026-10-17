//! Re-checking authorization before a long-running job commits its result (S4-013).
//!
//! A job is authorized when it is enqueued, but it may run minutes later. Before a worker commits
//! a result it calls [`recheck_before_commit`] inside the transaction that commits it. The helper
//! re-resolves the requesting principal's effective access to the job's resource
//! ([`crate::effective_roles::resolve_document_access`]) and refuses with
//! [`JobAuthorizationError::Revoked`] when the permission is gone: the principal was suspended or
//! removed from the tenant, the tenant was suspended, a direct membership, team membership or team
//! grant was removed, or the document left a state the permission allows.
//!
//! The rule mirrors `figcraft_cloud_auth::authz::decide` for the job's action (this crate sits
//! below `figcraft-cloud-auth`, so it is restated here and covered by the tests):
//! active principal, active tenant, active membership, then the document's combined state and the
//! effective role:
//!
//! | permission | states | least role |
//! |---|---|---|
//! | [`JobPermission::DocumentExport`] (`document.export`) | active, archived | viewer |
//! | [`JobPermission::DocumentView`] (`document.view`) | active, archived | viewer |
//! | [`JobPermission::DocumentEdit`] (`document.edit`) | active | editor |
//!
//! # Racing a revocation
//!
//! Before resolving, the helper takes `FOR SHARE` locks on every row the decision reads: the
//! tenant, the principal, the tenant membership, the document, its project and workspace, the
//! principal's direct workspace and document memberships, their team memberships and teams, and
//! the workspace's and document's team grants. Every revocation updates or deletes one of those
//! rows, so it either commits before the recheck (and the recheck sees it) or waits until the
//! job's transaction ends: the job's result can never be committed after a revocation that
//! committed first. Lock order (tenant first) matches the membership repositories, which lock the
//! tenant row first, so the two do not deadlock; any other deadlock is reported by PostgreSQL as
//! a retryable error (`Database`), and the worker retries.
//!
//! # Failing the job
//!
//! On `Revoked` the commit transaction must be rolled back (nothing of the result is kept); the
//! worker then marks the job failed with [`AUTHORIZATION_REVOKED`] in a new transaction: for
//! exports [`crate::exports::fail_export`] (which [`crate::exports::complete_export`] reports as
//! `ExportError::AuthorizationRevoked`), for any other job kind [`fail_job_authorization_revoked`].

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, ResourceState, Uuid};

use crate::TenantTransaction;
use crate::effective_roles::{ResourceAccess, resolve_document_access, resource_rank};

/// The `error_code` a job refused at commit is failed with.
pub const AUTHORIZATION_REVOKED: &str = "authorization_revoked";
/// Most team memberships of one principal locked by one recheck.
const MAX_LOCKED_TEAMS: i64 = 512;

/// The permission a job's result needs at commit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobPermission {
    DocumentExport,
    DocumentView,
    DocumentEdit,
}

impl JobPermission {
    const fn least_rank(self) -> u8 {
        match self {
            Self::DocumentExport | Self::DocumentView => 1,
            Self::DocumentEdit => 3,
        }
    }

    const fn allows_state(self, state: ResourceState) -> bool {
        match self {
            Self::DocumentExport | Self::DocumentView => matches!(state, ResourceState::Active | ResourceState::Archived),
            Self::DocumentEdit => matches!(state, ResourceState::Active),
        }
    }
}

/// Who asked for a job, on what, needing which permission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JobAuthorization {
    pub requested_by: PrincipalId,
    pub document: CloudDocumentId,
    pub permission: JobPermission,
}

/// Fixed, non-leaking categories for a recheck.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobAuthorizationError {
    /// The requester no longer holds the permission (or the document is gone): roll back and fail
    /// the job with [`AUTHORIZATION_REVOKED`].
    Revoked,
    /// The check could not run (including a lock timeout or deadlock); retry the commit.
    Database,
}

impl std::fmt::Display for JobAuthorizationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "job authorization error: {self:?}")
    }
}

impl std::error::Error for JobAuthorizationError {}

/// Whether resolved `access` still allows `permission` (the rule in the module docs).
#[must_use]
pub fn access_allows(access: &ResourceAccess, permission: JobPermission) -> bool {
    access.tenant.principal_active
        && access.tenant.tenant_state == ResourceState::Active
        && access.tenant.tenant_role.is_some()
        && permission.allows_state(access.state)
        && access.role.is_some_and(|role| {
            let rank = resource_rank(role);
            rank > 0 && rank >= permission.least_rank()
        })
}

/// Lock every row the decision for `job` reads (see the module docs).
async fn lock_decision_rows(transaction: &mut TenantTransaction<'_>, job: &JobAuthorization) -> Result<(), JobAuthorizationError> {
    let tenant = transaction.tenant_id().to_string();
    let principal = job.requested_by.to_string();
    let document = job.document.to_string();
    // Single-row locks first, in the membership repositories' order (tenant first).
    for statement in [
        "SELECT 1 FROM tenants WHERE id = $1::uuid FOR SHARE",
        "SELECT 1 FROM principals WHERE id = $2::uuid FOR SHARE",
        "SELECT 1 FROM tenant_members WHERE tenant_id = $1::uuid AND principal_id = $2::uuid FOR SHARE",
        "SELECT 1 FROM workspaces w WHERE w.tenant_id = $1::uuid AND w.id = (SELECT d.workspace_id FROM documents d WHERE d.tenant_id = $1::uuid AND d.id = $3::uuid) FOR SHARE",
        "SELECT 1 FROM projects p WHERE p.tenant_id = $1::uuid AND p.id = (SELECT d.project_id FROM documents d WHERE d.tenant_id = $1::uuid AND d.id = $3::uuid) FOR SHARE",
        "SELECT 1 FROM documents WHERE tenant_id = $1::uuid AND id = $3::uuid FOR SHARE",
        "SELECT 1 FROM workspace_members m WHERE m.tenant_id = $1::uuid AND m.principal_id = $2::uuid AND m.workspace_id = (SELECT d.workspace_id FROM documents d WHERE d.tenant_id = $1::uuid AND d.id = $3::uuid) FOR SHARE",
        "SELECT 1 FROM document_members WHERE tenant_id = $1::uuid AND principal_id = $2::uuid AND document_id = $3::uuid FOR SHARE",
    ] {
        sqlx::query(statement)
            .bind(&tenant)
            .bind(&principal)
            .bind(&document)
            .fetch_all(transaction.connection())
            .await
            .map_err(|_| JobAuthorizationError::Database)?;
    }
    // Bounded multi-row locks: the principal's team memberships, those teams, and the grants.
    for statement in [
        "SELECT 1 FROM team_members WHERE tenant_id = $1::uuid AND principal_id = $2::uuid ORDER BY team_id LIMIT $4 FOR SHARE",
        "SELECT 1 FROM teams t WHERE t.tenant_id = $1::uuid AND t.id IN (SELECT tm.team_id FROM team_members tm WHERE tm.tenant_id = $1::uuid AND tm.principal_id = $2::uuid) ORDER BY t.id LIMIT $4 FOR SHARE",
        "SELECT 1 FROM workspace_team_grants g WHERE g.tenant_id = $1::uuid AND g.workspace_id = (SELECT d.workspace_id FROM documents d WHERE d.tenant_id = $1::uuid AND d.id = $3::uuid) AND g.team_id IN (SELECT tm.team_id FROM team_members tm WHERE tm.tenant_id = $1::uuid AND tm.principal_id = $2::uuid) ORDER BY g.team_id LIMIT $4 FOR SHARE",
        "SELECT 1 FROM document_team_grants g WHERE g.tenant_id = $1::uuid AND g.document_id = $3::uuid AND g.team_id IN (SELECT tm.team_id FROM team_members tm WHERE tm.tenant_id = $1::uuid AND tm.principal_id = $2::uuid) ORDER BY g.team_id LIMIT $4 FOR SHARE",
    ] {
        sqlx::query(statement)
            .bind(&tenant)
            .bind(&principal)
            .bind(&document)
            .bind(MAX_LOCKED_TEAMS)
            .fetch_all(transaction.connection())
            .await
            .map_err(|_| JobAuthorizationError::Database)?;
    }
    Ok(())
}

/// Re-resolve `job`'s requester's access at commit time inside `transaction` (the one that will
/// commit the result). `Ok(())` means the result may be committed; `Revoked` means roll back and
/// fail the job with [`AUTHORIZATION_REVOKED`].
pub async fn recheck_before_commit(transaction: &mut TenantTransaction<'_>, job: &JobAuthorization) -> Result<(), JobAuthorizationError> {
    lock_decision_rows(transaction, job).await?;
    let access = resolve_document_access(transaction, job.requested_by, job.document).await.map_err(|_| JobAuthorizationError::Database)?;
    match access {
        Some(access) if access_allows(&access, job.permission) => Ok(()),
        _ => Err(JobAuthorizationError::Revoked),
    }
}

/// Fail queued or running job `job_id` (of any kind) in the transaction's tenant with
/// [`AUTHORIZATION_REVOKED`]: true when it was failed, false when it had already finished. The
/// caller commits. Exports use [`crate::exports::fail_export`] instead, which keeps the export row
/// and its job in step.
pub async fn fail_job_authorization_revoked(transaction: &mut TenantTransaction<'_>, job_id: Uuid) -> Result<bool, JobAuthorizationError> {
    let result = sqlx::query(concat!(
        "UPDATE jobs SET state = 'failed', error_code = $3, lease_owner = NULL, lease_expires_at = NULL, updated_at = now() ",
        "WHERE tenant_id = $1::uuid AND id = $2::uuid AND state IN ('queued', 'running')"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(job_id.to_string())
    .bind(AUTHORIZATION_REVOKED)
    .execute(transaction.connection())
    .await
    .map_err(|_| JobAuthorizationError::Database)?;
    Ok(result.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effective_roles::TenantAccess;
    use figcraft_cloud_domain::Role;

    fn access(state: ResourceState, role: Option<Role>) -> ResourceAccess {
        ResourceAccess {
            tenant: TenantAccess {
                principal_active: true,
                tenant_state: ResourceState::Active,
                tenant_role: Some(Role::Editor),
                permissions: Vec::new(),
            },
            state,
            role,
            sources: Vec::new(),
        }
    }

    #[test]
    fn the_rule_matches_the_authz_matrix_for_job_actions() {
        use JobPermission as P;
        use ResourceState as S;
        assert!(access_allows(&access(S::Active, Some(Role::Viewer)), P::DocumentExport));
        assert!(access_allows(&access(S::Archived, Some(Role::Viewer)), P::DocumentExport));
        assert!(!access_allows(&access(S::Deleting, Some(Role::WorkspaceAdmin)), P::DocumentExport));
        assert!(!access_allows(&access(S::Active, None), P::DocumentView));
        assert!(!access_allows(&access(S::Active, Some(Role::TeamManager)), P::DocumentView));
        assert!(!access_allows(&access(S::Active, Some(Role::Commenter)), P::DocumentEdit));
        assert!(access_allows(&access(S::Active, Some(Role::Editor)), P::DocumentEdit));
        assert!(!access_allows(&access(S::Archived, Some(Role::Editor)), P::DocumentEdit));
        let mut suspended = access(S::Active, Some(Role::WorkspaceAdmin));
        suspended.tenant.tenant_state = S::Suspended;
        assert!(!access_allows(&suspended, P::DocumentView));
        let mut inactive = access(S::Active, Some(Role::WorkspaceAdmin));
        inactive.tenant.principal_active = false;
        assert!(!access_allows(&inactive, P::DocumentView));
        let mut removed = access(S::Active, Some(Role::WorkspaceAdmin));
        removed.tenant.tenant_role = None;
        assert!(!access_allows(&removed, P::DocumentView));
        assert!(crate::exports::valid_error_code(AUTHORIZATION_REVOKED));
    }
}

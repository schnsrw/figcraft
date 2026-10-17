//! Workspace, project and document administration the HTTP API needs beyond [`crate::metadata`]
//! (S4-019): project lookup by ID, workspace and project rename and lifecycle transitions,
//! keyset-paged document listing, the visible-workspace listing, and listings of direct members
//! and document team grants.
//!
//! Like every tenant-owned repository, the tenant predicate comes from the [`TenantTransaction`].
//! Every listing is bounded ([`PageSize`] or [`MAX_LISTED`]). Authorization is the caller's job
//! (`figcraft_cloud_auth::authz::decide` over `effective_roles`); the visible-workspace listing is
//! only a pre-filter that the caller re-checks per row.

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, ProjectId, ResourceState, Role, Uuid, WorkspaceId};

use crate::TenantTransaction;
use crate::memberships::parse_role;
use crate::metadata::{DocumentRecord, Page, PageSize, ProjectRecord, RepositoryError, WorkspaceRecord, page, state};

/// Most members or grants one listing call returns.
pub const MAX_LISTED: i64 = 100;

/// A lifecycle change on a workspace or project.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Transition {
    /// `active` → `archived`.
    Archive,
    /// `archived` or `deleting` → `active`.
    Restore,
    /// `active` or `archived` → `deleting` (the soft-delete grace period).
    SoftDelete,
}

impl Transition {
    /// The state the transition settles in (repeating it is idempotent).
    #[must_use]
    pub const fn settled(self) -> ResourceState {
        match self {
            Self::Archive => ResourceState::Archived,
            Self::Restore => ResourceState::Active,
            Self::SoftDelete => ResourceState::Deleting,
        }
    }
}

fn database_error(error: &sqlx::Error) -> RepositoryError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23505") => RepositoryError::Conflict,
        Some("23503" | "23514") => RepositoryError::Constraint,
        _ => RepositoryError::Database,
    }
}

type WorkspaceRow = (String, String, Option<String>, String, String);
type ProjectRow = (String, String, String, Option<String>, String, String);
type DocumentRow = (String, String, String, String, String, String);

fn team(value: Option<String>) -> Result<Option<Uuid>, RepositoryError> {
    value.map(|value| Uuid::parse_str(&value).map_err(|_| RepositoryError::CorruptRow)).transpose()
}

fn workspace(row: WorkspaceRow) -> Result<WorkspaceRecord, RepositoryError> {
    Ok(WorkspaceRecord {
        id: row.0.parse().map_err(|_| RepositoryError::CorruptRow)?,
        tenant_id: row.1.parse().map_err(|_| RepositoryError::CorruptRow)?,
        owning_team_id: team(row.2)?,
        name: row.3,
        state: state(&row.4)?,
    })
}

fn project(row: ProjectRow) -> Result<ProjectRecord, RepositoryError> {
    Ok(ProjectRecord {
        id: row.0.parse().map_err(|_| RepositoryError::CorruptRow)?,
        tenant_id: row.1.parse().map_err(|_| RepositoryError::CorruptRow)?,
        workspace_id: row.2.parse().map_err(|_| RepositoryError::CorruptRow)?,
        owning_team_id: team(row.3)?,
        name: row.4,
        state: state(&row.5)?,
    })
}

fn document(row: DocumentRow) -> Result<DocumentRecord, RepositoryError> {
    Ok(DocumentRecord {
        id: row.0.parse().map_err(|_| RepositoryError::CorruptRow)?,
        tenant_id: row.1.parse().map_err(|_| RepositoryError::CorruptRow)?,
        workspace_id: row.2.parse().map_err(|_| RepositoryError::CorruptRow)?,
        project_id: row.3.parse().map_err(|_| RepositoryError::CorruptRow)?,
        name: row.4,
        state: state(&row.5)?,
    })
}

/// A project by its ID alone (the API addresses projects without their workspace).
pub async fn get_project_by_id(transaction: &mut TenantTransaction<'_>, id: ProjectId) -> Result<Option<ProjectRecord>, RepositoryError> {
    let row: Option<ProjectRow> = sqlx::query_as(
        "SELECT id::text, tenant_id::text, workspace_id::text, owning_team_id::text, name, state \
         FROM projects WHERE tenant_id = $1::uuid AND id = $2::uuid",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    row.map(project).transpose()
}

async fn get_workspace(transaction: &mut TenantTransaction<'_>, id: WorkspaceId) -> Result<Option<WorkspaceRecord>, RepositoryError> {
    crate::metadata::get_workspace(transaction, id).await
}

/// Why a conditional update matched nothing: missing (`None`), already settled, or another state.
fn settle<T>(existing: Option<T>, current: impl Fn(&T) -> ResourceState, settled: ResourceState) -> Result<Option<T>, RepositoryError> {
    match existing {
        Some(record) if current(&record) == settled => Ok(Some(record)),
        Some(_) => Err(RepositoryError::InvalidState),
        None => Ok(None),
    }
}

/// Rename a workspace (from `active` or `archived`). `None`: no such workspace.
pub async fn rename_workspace(
    transaction: &mut TenantTransaction<'_>,
    id: WorkspaceId,
    name: &str,
) -> Result<Option<WorkspaceRecord>, RepositoryError> {
    let row: Option<WorkspaceRow> = sqlx::query_as(
        "UPDATE workspaces SET name = $3, updated_at = now() \
         WHERE tenant_id = $1::uuid AND id = $2::uuid AND state IN ('active', 'archived') \
         RETURNING id::text, tenant_id::text, owning_team_id::text, name, state",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(id.to_string())
    .bind(name)
    .fetch_optional(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    if let Some(row) = row {
        return Ok(Some(workspace(row)?));
    }
    match get_workspace(transaction, id).await? {
        Some(_) => Err(RepositoryError::InvalidState),
        None => Ok(None),
    }
}

/// Apply `transition` to a workspace. Idempotent when it is already in the settled state; any
/// other state is [`RepositoryError::InvalidState`]. `None`: no such workspace.
pub async fn transition_workspace(
    transaction: &mut TenantTransaction<'_>,
    id: WorkspaceId,
    transition: Transition,
) -> Result<Option<WorkspaceRecord>, RepositoryError> {
    let query = match transition {
        Transition::Archive => sqlx::query_as::<_, WorkspaceRow>(
            "UPDATE workspaces SET state = 'archived', updated_at = now() \
             WHERE tenant_id = $1::uuid AND id = $2::uuid AND state = 'active' \
             RETURNING id::text, tenant_id::text, owning_team_id::text, name, state",
        ),
        Transition::Restore => sqlx::query_as::<_, WorkspaceRow>(
            "UPDATE workspaces SET state = 'active', updated_at = now() \
             WHERE tenant_id = $1::uuid AND id = $2::uuid AND state IN ('archived', 'deleting') \
             RETURNING id::text, tenant_id::text, owning_team_id::text, name, state",
        ),
        Transition::SoftDelete => sqlx::query_as::<_, WorkspaceRow>(
            "UPDATE workspaces SET state = 'deleting', updated_at = now() \
             WHERE tenant_id = $1::uuid AND id = $2::uuid AND state IN ('active', 'archived') \
             RETURNING id::text, tenant_id::text, owning_team_id::text, name, state",
        ),
    };
    let row = query
        .bind(transaction.tenant_id().to_string())
        .bind(id.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    if let Some(row) = row {
        return Ok(Some(workspace(row)?));
    }
    settle(get_workspace(transaction, id).await?, |record| record.state, transition.settled())
}

/// Rename a project (from `active` or `archived`). `None`: no such project.
pub async fn rename_project(transaction: &mut TenantTransaction<'_>, id: ProjectId, name: &str) -> Result<Option<ProjectRecord>, RepositoryError> {
    let row: Option<ProjectRow> = sqlx::query_as(
        "UPDATE projects SET name = $3, updated_at = now() \
         WHERE tenant_id = $1::uuid AND id = $2::uuid AND state IN ('active', 'archived') \
         RETURNING id::text, tenant_id::text, workspace_id::text, owning_team_id::text, name, state",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(id.to_string())
    .bind(name)
    .fetch_optional(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    if let Some(row) = row {
        return Ok(Some(project(row)?));
    }
    match get_project_by_id(transaction, id).await? {
        Some(_) => Err(RepositoryError::InvalidState),
        None => Ok(None),
    }
}

/// Apply `transition` to a project (see [`transition_workspace`]).
pub async fn transition_project(
    transaction: &mut TenantTransaction<'_>,
    id: ProjectId,
    transition: Transition,
) -> Result<Option<ProjectRecord>, RepositoryError> {
    let query = match transition {
        Transition::Archive => sqlx::query_as::<_, ProjectRow>(
            "UPDATE projects SET state = 'archived', updated_at = now() \
             WHERE tenant_id = $1::uuid AND id = $2::uuid AND state = 'active' \
             RETURNING id::text, tenant_id::text, workspace_id::text, owning_team_id::text, name, state",
        ),
        Transition::Restore => sqlx::query_as::<_, ProjectRow>(
            "UPDATE projects SET state = 'active', updated_at = now() \
             WHERE tenant_id = $1::uuid AND id = $2::uuid AND state IN ('archived', 'deleting') \
             RETURNING id::text, tenant_id::text, workspace_id::text, owning_team_id::text, name, state",
        ),
        Transition::SoftDelete => sqlx::query_as::<_, ProjectRow>(
            "UPDATE projects SET state = 'deleting', updated_at = now() \
             WHERE tenant_id = $1::uuid AND id = $2::uuid AND state IN ('active', 'archived') \
             RETURNING id::text, tenant_id::text, workspace_id::text, owning_team_id::text, name, state",
        ),
    };
    let row = query
        .bind(transaction.tenant_id().to_string())
        .bind(id.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    if let Some(row) = row {
        return Ok(Some(project(row)?));
    }
    settle(get_project_by_id(transaction, id).await?, |record| record.state, transition.settled())
}

/// One keyset page of a project's documents in every state except `deleted`, by ID.
pub async fn list_documents(
    transaction: &mut TenantTransaction<'_>,
    project_id: ProjectId,
    after: Option<CloudDocumentId>,
    size: PageSize,
) -> Result<Page<DocumentRecord, CloudDocumentId>, RepositoryError> {
    let rows: Vec<DocumentRow> = sqlx::query_as(
        "SELECT id::text, tenant_id::text, workspace_id::text, project_id::text, name, state \
         FROM documents WHERE tenant_id = $1::uuid AND project_id = $2::uuid AND state <> 'deleted' \
           AND ($3::uuid IS NULL OR id > $3::uuid) \
         ORDER BY id LIMIT $4",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(project_id.to_string())
    .bind(after.map(|id| id.to_string()))
    .bind(size.query_limit())
    .fetch_all(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    Ok(page(rows.into_iter().map(document).collect::<Result<Vec<_>, _>>()?, size, |item| item.id))
}

/// One keyset page of the workspaces `principal_id` may be able to see: every workspace when
/// `every_workspace` (tenant owners and admins), otherwise those with a direct membership or a
/// grant to a team the principal is an active member of. `deleted` workspaces are never listed.
/// A pre-filter: the caller still decides each row.
pub async fn list_visible_workspaces(
    transaction: &mut TenantTransaction<'_>,
    principal_id: PrincipalId,
    every_workspace: bool,
    after: Option<WorkspaceId>,
    size: PageSize,
) -> Result<Page<WorkspaceRecord, WorkspaceId>, RepositoryError> {
    let rows: Vec<WorkspaceRow> = sqlx::query_as(
        "SELECT w.id::text, w.tenant_id::text, w.owning_team_id::text, w.name, w.state FROM workspaces w \
         WHERE w.tenant_id = $1::uuid AND w.state <> 'deleted' AND ($3::uuid IS NULL OR w.id > $3::uuid) \
           AND ($4::boolean \
             OR EXISTS (SELECT 1 FROM workspace_members m \
                        WHERE m.tenant_id = w.tenant_id AND m.workspace_id = w.id AND m.principal_id = $2::uuid) \
             OR EXISTS (SELECT 1 FROM workspace_team_grants g \
                        JOIN teams t ON t.tenant_id = g.tenant_id AND t.id = g.team_id AND t.state = 'active' \
                        JOIN team_members tm ON tm.tenant_id = g.tenant_id AND tm.team_id = g.team_id AND tm.state = 'active' \
                        WHERE g.tenant_id = w.tenant_id AND g.workspace_id = w.id AND tm.principal_id = $2::uuid)) \
         ORDER BY w.id LIMIT $5",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(principal_id.to_string())
    .bind(after.map(|id| id.to_string()))
    .bind(every_workspace)
    .bind(size.query_limit())
    .fetch_all(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    Ok(page(rows.into_iter().map(workspace).collect::<Result<Vec<_>, _>>()?, size, |item| item.id))
}

/// A direct member or a team grant: who (principal or team ID) and which role.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Grant<Id> {
    pub id: Id,
    pub role: Role,
}

fn grants<Id: std::str::FromStr>(rows: Vec<(String, String)>) -> Result<Vec<Grant<Id>>, RepositoryError> {
    rows.into_iter()
        .map(|(id, role)| {
            Ok(Grant { id: id.parse().map_err(|_| RepositoryError::CorruptRow)?, role: parse_role(&role).ok_or(RepositoryError::CorruptRow)? })
        })
        .collect()
}

/// A workspace's direct members by principal ID after `after`, at most [`MAX_LISTED`].
pub async fn list_workspace_members(
    transaction: &mut TenantTransaction<'_>,
    workspace_id: WorkspaceId,
    after: Option<PrincipalId>,
) -> Result<Vec<Grant<PrincipalId>>, RepositoryError> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT principal_id::text, role FROM workspace_members \
         WHERE tenant_id = $1::uuid AND workspace_id = $2::uuid AND ($3::uuid IS NULL OR principal_id > $3::uuid) \
         ORDER BY principal_id LIMIT $4",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(workspace_id.to_string())
    .bind(after.map(|id| id.to_string()))
    .bind(MAX_LISTED)
    .fetch_all(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    grants(rows)
}

/// A document's direct members by principal ID after `after`, at most [`MAX_LISTED`].
pub async fn list_document_members(
    transaction: &mut TenantTransaction<'_>,
    document_id: CloudDocumentId,
    after: Option<PrincipalId>,
) -> Result<Vec<Grant<PrincipalId>>, RepositoryError> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT principal_id::text, role FROM document_members \
         WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND ($3::uuid IS NULL OR principal_id > $3::uuid) \
         ORDER BY principal_id LIMIT $4",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(document_id.to_string())
    .bind(after.map(|id| id.to_string()))
    .bind(MAX_LISTED)
    .fetch_all(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    grants(rows)
}

/// A document's team grants by team ID after `after`, at most [`MAX_LISTED`].
pub async fn list_document_team_grants(
    transaction: &mut TenantTransaction<'_>,
    document_id: CloudDocumentId,
    after: Option<Uuid>,
) -> Result<Vec<Grant<Uuid>>, RepositoryError> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT team_id::text, role FROM document_team_grants \
         WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND ($3::uuid IS NULL OR team_id > $3::uuid) \
         ORDER BY team_id LIMIT $4",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(document_id.to_string())
    .bind(after.map(|id| id.to_string()))
    .bind(MAX_LISTED)
    .fetch_all(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    grants(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions_settle_idempotently_and_reject_other_states() {
        let current = |state: &ResourceState| *state;
        assert_eq!(settle(None, current, ResourceState::Archived), Ok(None));
        assert_eq!(settle(Some(ResourceState::Archived), current, Transition::Archive.settled()), Ok(Some(ResourceState::Archived)));
        assert_eq!(settle(Some(ResourceState::Deleting), current, Transition::Archive.settled()), Err(RepositoryError::InvalidState));
        assert_eq!(Transition::Restore.settled(), ResourceState::Active);
        assert_eq!(Transition::SoftDelete.settled(), ResourceState::Deleting);
    }
}

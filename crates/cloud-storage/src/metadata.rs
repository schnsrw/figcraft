//! Tenant, workspace and project metadata repositories.
//!
//! Every tenant-owned query derives its predicate from `TenantTransaction`, not a caller-supplied
//! tenant parameter. Tenant discovery is the exception: it is scoped by an authenticated
//! principal's active membership and must run before a tenant is selected.

use figcraft_cloud_domain::{PrincipalId, ProjectId, ResourceState, TenantId, Uuid, WorkspaceId};
use sqlx::PgPool;
use std::fmt;

use crate::TenantTransaction;

/// Fixed, non-leaking categories for metadata repository failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryError {
    InvalidPageSize,
    Conflict,
    Constraint,
    Database,
    CorruptRow,
}

impl fmt::Display for RepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "metadata repository error: {self:?}")
    }
}

impl std::error::Error for RepositoryError {}

fn database_error(error: sqlx::Error) -> RepositoryError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23505") => RepositoryError::Conflict,
        Some("23503" | "23514") => RepositoryError::Constraint,
        _ => RepositoryError::Database,
    }
}

/// A strictly bounded list size. The maximum is 100 rows, including for internal callers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageSize(u8);

impl PageSize {
    pub const MAX: u32 = 100;

    pub fn new(requested: u32) -> Result<Self, RepositoryError> {
        if requested == 0 || requested > Self::MAX {
            return Err(RepositoryError::InvalidPageSize);
        }
        Ok(Self(requested as u8))
    }

    fn query_limit(self) -> i64 {
        i64::from(self.0) + 1
    }

    fn as_usize(self) -> usize {
        usize::from(self.0)
    }
}

/// One keyset page. The cursor is the final returned row's typed ID, not an HTTP wire cursor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Page<T, Id> {
    pub items: Vec<T>,
    pub next_cursor: Option<Id>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TenantRecord {
    pub id: TenantId,
    pub slug: String,
    pub name: String,
    pub state: ResourceState,
    pub plan_id: String,
    pub home_region: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceRecord {
    pub id: WorkspaceId,
    pub tenant_id: TenantId,
    pub owning_team_id: Option<Uuid>,
    pub name: String,
    pub state: ResourceState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectRecord {
    pub id: ProjectId,
    pub tenant_id: TenantId,
    pub workspace_id: WorkspaceId,
    pub owning_team_id: Option<Uuid>,
    pub name: String,
    pub state: ResourceState,
}

type TenantRow = (String, String, String, String, String, String);
type WorkspaceRow = (String, String, Option<String>, String, String);
type ProjectRow = (String, String, String, Option<String>, String, String);

fn state(value: &str) -> Result<ResourceState, RepositoryError> {
    match value {
        "pending" => Ok(ResourceState::Pending),
        "active" => Ok(ResourceState::Active),
        "suspended" => Ok(ResourceState::Suspended),
        "archived" => Ok(ResourceState::Archived),
        "deleting" => Ok(ResourceState::Deleting),
        "deleted" => Ok(ResourceState::Deleted),
        _ => Err(RepositoryError::CorruptRow),
    }
}

fn tenant(row: TenantRow) -> Result<TenantRecord, RepositoryError> {
    Ok(TenantRecord {
        id: row.0.parse().map_err(|_| RepositoryError::CorruptRow)?,
        slug: row.1,
        name: row.2,
        state: state(&row.3)?,
        plan_id: row.4,
        home_region: row.5,
    })
}

fn workspace(row: WorkspaceRow) -> Result<WorkspaceRecord, RepositoryError> {
    Ok(WorkspaceRecord {
        id: row.0.parse().map_err(|_| RepositoryError::CorruptRow)?,
        tenant_id: row.1.parse().map_err(|_| RepositoryError::CorruptRow)?,
        owning_team_id: row.2.map(|value| Uuid::parse_str(&value).map_err(|_| RepositoryError::CorruptRow)).transpose()?,
        name: row.3,
        state: state(&row.4)?,
    })
}

fn project(row: ProjectRow) -> Result<ProjectRecord, RepositoryError> {
    Ok(ProjectRecord {
        id: row.0.parse().map_err(|_| RepositoryError::CorruptRow)?,
        tenant_id: row.1.parse().map_err(|_| RepositoryError::CorruptRow)?,
        workspace_id: row.2.parse().map_err(|_| RepositoryError::CorruptRow)?,
        owning_team_id: row.3.map(|value| Uuid::parse_str(&value).map_err(|_| RepositoryError::CorruptRow)).transpose()?,
        name: row.4,
        state: state(&row.5)?,
    })
}

fn page<T, Id: Copy>(mut items: Vec<T>, size: PageSize, id: impl Fn(&T) -> Id) -> Page<T, Id> {
    let has_more = items.len() > size.as_usize();
    items.truncate(size.as_usize());
    let next_cursor = if has_more { items.last().map(id) } else { None };
    Page { items, next_cursor }
}

/// Create a tenant in its own scoped transaction. Authorization and owner membership are caller
/// responsibilities until identity provisioning lands; a failed caller transaction must roll back.
pub async fn create_tenant(
    transaction: &mut TenantTransaction<'_>,
    slug: &str,
    name: &str,
    plan_id: &str,
    home_region: &str,
) -> Result<TenantRecord, RepositoryError> {
    let row: TenantRow = sqlx::query_as(
        "INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $2, $3, $4, $5) \
         RETURNING id::text, slug, name, state, plan_id, home_region",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(slug)
    .bind(name)
    .bind(plan_id)
    .bind(home_region)
    .fetch_one(transaction.connection())
    .await
    .map_err(database_error)?;
    tenant(row)
}

pub async fn get_tenant(transaction: &mut TenantTransaction<'_>) -> Result<Option<TenantRecord>, RepositoryError> {
    let row: Option<TenantRow> = sqlx::query_as("SELECT id::text, slug, name, state, plan_id, home_region FROM tenants WHERE id = $1::uuid")
        .bind(transaction.tenant_id().to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(database_error)?;
    row.map(tenant).transpose()
}

/// Discover only tenants where the trusted principal has active membership. This is the
/// pre-selection query; no tenant-scoped transaction exists yet.
pub async fn list_tenants_for_principal(
    pool: &PgPool,
    principal_id: PrincipalId,
    after: Option<TenantId>,
    size: PageSize,
) -> Result<Page<TenantRecord, TenantId>, RepositoryError> {
    let rows: Vec<TenantRow> = sqlx::query_as(
        "SELECT t.id::text, t.slug, t.name, t.state, t.plan_id, t.home_region \
         FROM tenants t JOIN tenant_members m ON m.tenant_id = t.id \
         WHERE m.principal_id = $1::uuid AND m.state = 'active' \
           AND ($2::uuid IS NULL OR t.id > $2::uuid) \
         ORDER BY t.id LIMIT $3",
    )
    .bind(principal_id.to_string())
    .bind(after.map(|id| id.to_string()))
    .bind(size.query_limit())
    .fetch_all(pool)
    .await
    .map_err(database_error)?;
    Ok(page(rows.into_iter().map(tenant).collect::<Result<Vec<_>, _>>()?, size, |item| item.id))
}

pub async fn create_workspace(
    transaction: &mut TenantTransaction<'_>,
    id: WorkspaceId,
    name: &str,
    owning_team_id: Option<Uuid>,
) -> Result<WorkspaceRecord, RepositoryError> {
    let row: WorkspaceRow = sqlx::query_as(
        "INSERT INTO workspaces (id, tenant_id, owning_team_id, name) VALUES ($1::uuid, $2::uuid, $3::uuid, $4) \
         RETURNING id::text, tenant_id::text, owning_team_id::text, name, state",
    )
    .bind(id.to_string())
    .bind(transaction.tenant_id().to_string())
    .bind(owning_team_id.map(|id| id.to_string()))
    .bind(name)
    .fetch_one(transaction.connection())
    .await
    .map_err(database_error)?;
    workspace(row)
}

pub async fn get_workspace(transaction: &mut TenantTransaction<'_>, id: WorkspaceId) -> Result<Option<WorkspaceRecord>, RepositoryError> {
    let row: Option<WorkspaceRow> = sqlx::query_as(
        "SELECT id::text, tenant_id::text, owning_team_id::text, name, state \
         FROM workspaces WHERE tenant_id = $1::uuid AND id = $2::uuid",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    row.map(workspace).transpose()
}

pub async fn list_workspaces(
    transaction: &mut TenantTransaction<'_>,
    after: Option<WorkspaceId>,
    size: PageSize,
) -> Result<Page<WorkspaceRecord, WorkspaceId>, RepositoryError> {
    let rows: Vec<WorkspaceRow> = sqlx::query_as(
        "SELECT id::text, tenant_id::text, owning_team_id::text, name, state \
         FROM workspaces WHERE tenant_id = $1::uuid AND ($2::uuid IS NULL OR id > $2::uuid) \
         ORDER BY id LIMIT $3",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(after.map(|id| id.to_string()))
    .bind(size.query_limit())
    .fetch_all(transaction.connection())
    .await
    .map_err(database_error)?;
    Ok(page(rows.into_iter().map(workspace).collect::<Result<Vec<_>, _>>()?, size, |item| item.id))
}

pub async fn create_project(
    transaction: &mut TenantTransaction<'_>,
    id: ProjectId,
    workspace_id: WorkspaceId,
    name: &str,
    owning_team_id: Option<Uuid>,
) -> Result<ProjectRecord, RepositoryError> {
    let row: ProjectRow = sqlx::query_as(
        "INSERT INTO projects (id, tenant_id, workspace_id, owning_team_id, name) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, $5) \
         RETURNING id::text, tenant_id::text, workspace_id::text, owning_team_id::text, name, state",
    )
    .bind(id.to_string())
    .bind(transaction.tenant_id().to_string())
    .bind(workspace_id.to_string())
    .bind(owning_team_id.map(|id| id.to_string()))
    .bind(name)
    .fetch_one(transaction.connection())
    .await
    .map_err(database_error)?;
    project(row)
}

pub async fn get_project(
    transaction: &mut TenantTransaction<'_>,
    workspace_id: WorkspaceId,
    id: ProjectId,
) -> Result<Option<ProjectRecord>, RepositoryError> {
    let row: Option<ProjectRow> = sqlx::query_as(
        "SELECT id::text, tenant_id::text, workspace_id::text, owning_team_id::text, name, state \
         FROM projects WHERE tenant_id = $1::uuid AND workspace_id = $2::uuid AND id = $3::uuid",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(workspace_id.to_string())
    .bind(id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(database_error)?;
    row.map(project).transpose()
}

pub async fn list_projects(
    transaction: &mut TenantTransaction<'_>,
    workspace_id: WorkspaceId,
    after: Option<ProjectId>,
    size: PageSize,
) -> Result<Page<ProjectRecord, ProjectId>, RepositoryError> {
    let rows: Vec<ProjectRow> = sqlx::query_as(
        "SELECT id::text, tenant_id::text, workspace_id::text, owning_team_id::text, name, state \
         FROM projects WHERE tenant_id = $1::uuid AND workspace_id = $2::uuid \
           AND ($3::uuid IS NULL OR id > $3::uuid) \
         ORDER BY id LIMIT $4",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(workspace_id.to_string())
    .bind(after.map(|id| id.to_string()))
    .bind(size.query_limit())
    .fetch_all(transaction.connection())
    .await
    .map_err(database_error)?;
    Ok(page(rows.into_iter().map(project).collect::<Result<Vec<_>, _>>()?, size, |item| item.id))
}

#[cfg(test)]
mod tests {
    use super::{PageSize, RepositoryError, page};

    #[test]
    fn page_sizes_are_strictly_bounded() {
        assert_eq!(PageSize::new(0), Err(RepositoryError::InvalidPageSize));
        assert_eq!(PageSize::new(101), Err(RepositoryError::InvalidPageSize));
        assert_eq!(PageSize::new(u32::MAX), Err(RepositoryError::InvalidPageSize));
        assert_eq!(PageSize::new(1), Ok(PageSize(1)));
        assert_eq!(PageSize::new(100), Ok(PageSize(100)));
    }

    #[test]
    fn next_cursor_is_only_returned_when_more_rows_exist() {
        let first = page(vec![1, 2, 3], PageSize(2), |item| *item);
        assert_eq!(first.items, [1, 2]);
        assert_eq!(first.next_cursor, Some(2));
        let final_page = page(vec![3], PageSize(2), |item| *item);
        assert_eq!(final_page.items, [3]);
        assert_eq!(final_page.next_cursor, None);
    }
}

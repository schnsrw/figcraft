//! S6-052 team ownership and project sharing, resolved against PostgreSQL (skipped cleanly without
//! FIGCRAFT_TEST_DATABASE_URL_FILE). The role arithmetic itself is unit-tested without a database in
//! `effective_roles.rs`.

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, ProjectId, Role, TenantId, Uuid, WorkspaceId};
use figcraft_cloud_storage::effective_roles::{RoleSource, resolve_document_access, resolve_project_access, resolve_workspace_access};
use figcraft_cloud_storage::memberships::add_tenant_member;
use figcraft_cloud_storage::metadata::{create_document, create_project, create_workspace};
use figcraft_cloud_storage::teams::{
    GrantTarget, OwnedResource, TeamGrant, TeamRole, WorkspaceGrantRole, add_team_member, archive_team, create_team, grant_to_team,
    list_project_team_grants, remove_team_member, revoke_team_grant, set_owning_team,
};
use figcraft_cloud_storage::tenants::{NewTenant, create_tenant_with_owner};
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use sqlx::postgres::{PgPool, PgPoolOptions};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

async fn pool() -> TestResult<Option<PgPool>> {
    let Ok(file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; team project probe skipped");
        return Ok(None);
    };
    let url = std::fs::read_to_string(file)?;
    Ok(Some(PgPoolOptions::new().max_connections(4).connect(url.trim_end()).await?))
}

async fn person(pool: &PgPool) -> TestResult<PrincipalId> {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'p4 person')").bind(id.to_string()).execute(pool).await?;
    Ok(id.try_into().map_err(|_| "nil")?)
}

fn err<E: std::fmt::Debug>(error: E) -> Box<dyn std::error::Error + Send + Sync> {
    format!("{error:?}").into()
}

struct World {
    pool: PgPool,
    tenant: TenantId,
    admin: PrincipalId,
    lead: PrincipalId,
    editor: PrincipalId,
    viewer: PrincipalId,
    other_team_member: PrincipalId,
    guest: PrincipalId,
    outsider: PrincipalId,
    team: Uuid,
    other_team: Uuid,
    workspace: WorkspaceId,
    project: ProjectId,
    document: CloudDocumentId,
}

async fn world(pool: PgPool) -> TestResult<World> {
    let (admin, lead, editor, viewer, other_team_member, guest, outsider) = (
        person(&pool).await?,
        person(&pool).await?,
        person(&pool).await?,
        person(&pool).await?,
        person(&pool).await?,
        person(&pool).await?,
        person(&pool).await?,
    );
    let tenant = TenantId::random();
    let mut tx = begin_tenant_transaction(&pool, tenant).await.map_err(err)?;
    let slug = format!("p4-{}", &Uuid::new_v4().simple().to_string()[..12]);
    create_tenant_with_owner(&mut tx, NewTenant { slug: &slug, name: "P4", plan_id: "team", home_region: "local" }, admin).await.map_err(err)?;
    for (principal, role) in
        [(lead, Role::Editor), (editor, Role::Editor), (viewer, Role::Editor), (other_team_member, Role::Editor), (guest, Role::Viewer)]
    {
        add_tenant_member(&mut tx, principal, role).await.map_err(err)?;
    }
    let team = Uuid::new_v4();
    let other_team = Uuid::new_v4();
    create_team(&mut tx, team, &format!("a-{}", &slug[3..]), "Design", lead).await.map_err(err)?;
    create_team(&mut tx, other_team, &format!("b-{}", &slug[3..]), "Marketing", admin).await.map_err(err)?;
    add_team_member(&mut tx, team, editor, TeamRole::Editor, lead).await.map_err(err)?;
    add_team_member(&mut tx, team, viewer, TeamRole::Viewer, lead).await.map_err(err)?;
    add_team_member(&mut tx, team, guest, TeamRole::Editor, lead).await.map_err(err)?;
    add_team_member(&mut tx, other_team, other_team_member, TeamRole::Editor, admin).await.map_err(err)?;
    let (workspace, project, document) = (WorkspaceId::random(), ProjectId::random(), CloudDocumentId::random());
    create_workspace(&mut tx, workspace, "p4 space", None).await.map_err(err)?;
    create_project(&mut tx, project, workspace, "p4 project", None).await.map_err(err)?;
    create_document(&mut tx, document, workspace, project, "p4 document").await.map_err(err)?;
    tx.commit().await?;
    Ok(World { pool, tenant, admin, lead, editor, viewer, other_team_member, guest, outsider, team, other_team, workspace, project, document })
}

impl World {
    async fn tx(&self) -> TestResult<TenantTransaction<'_>> {
        begin_tenant_transaction(&self.pool, self.tenant).await.map_err(err)
    }

    /// The role and sources of `who` on the document.
    async fn on_document(&self, who: PrincipalId) -> TestResult<(Option<Role>, Vec<RoleSource>)> {
        let mut tx = self.tx().await?;
        let access = resolve_document_access(&mut tx, who, self.document).await.map_err(err)?.ok_or("document")?;
        let _ = tx.rollback().await;
        Ok((access.role, access.sources))
    }
}

#[tokio::test]
async fn nobody_reaches_a_project_until_a_team_owns_or_is_granted_it() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let w = world(pool).await?;
    for who in [w.lead, w.editor, w.viewer, w.other_team_member, w.guest, w.outsider] {
        assert_eq!(w.on_document(who).await?, (None, vec![]), "deny by default");
    }
    assert_eq!(w.on_document(w.admin).await?.0, Some(Role::WorkspaceAdmin), "the tenant owner administers everything");
    Ok(())
}

#[tokio::test]
async fn a_team_that_owns_a_project_gives_its_members_access_by_team_role() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let w = world(pool).await?;
    let mut tx = w.tx().await?;
    set_owning_team(&mut tx, OwnedResource::Project(w.project.as_uuid()), Some(w.team), w.admin).await.map_err(err)?;
    tx.commit().await?;

    // The lead administers; an editor edits; a viewer reads; the guest (a tenant viewer) is capped.
    assert_eq!(w.on_document(w.lead).await?, (Some(Role::WorkspaceAdmin), vec![RoleSource::ProjectOwnerTeam]));
    assert_eq!(w.on_document(w.editor).await?, (Some(Role::Editor), vec![RoleSource::ProjectOwnerTeam]));
    assert_eq!(w.on_document(w.viewer).await?.0, Some(Role::Viewer));
    assert_eq!(w.on_document(w.guest).await?.0, Some(Role::Viewer), "a tenant guest never exceeds viewer");
    // Other teams and outsiders are still out; the project and its parent behave the same way.
    assert_eq!(w.on_document(w.other_team_member).await?.0, None);
    assert_eq!(w.on_document(w.outsider).await?.0, None);
    let mut tx = w.tx().await?;
    let on_project = resolve_project_access(&mut tx, w.editor, w.project).await.map_err(err)?.ok_or("project")?;
    assert_eq!(on_project.role, Some(Role::Editor));
    let on_workspace = resolve_workspace_access(&mut tx, w.editor, w.workspace).await.map_err(err)?.ok_or("workspace")?;
    assert_eq!(on_workspace.role, None, "owning a project does not open the workspace around it");
    // Leaving the team, or the team being archived, takes the access away at once.
    remove_team_member(&mut tx, w.team, w.editor, w.lead).await.map_err(err)?;
    tx.commit().await?;
    assert_eq!(w.on_document(w.editor).await?.0, None);
    let mut tx = w.tx().await?;
    archive_team(&mut tx, w.team, w.admin).await.map_err(err)?;
    tx.commit().await?;
    assert_eq!(w.on_document(w.lead).await?.0, None, "an archived team grants nothing");
    Ok(())
}

#[tokio::test]
async fn owning_a_workspace_covers_every_project_in_it() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let w = world(pool).await?;
    let mut tx = w.tx().await?;
    set_owning_team(&mut tx, OwnedResource::Workspace(w.workspace), Some(w.team), w.admin).await.map_err(err)?;
    tx.commit().await?;
    assert_eq!(w.on_document(w.lead).await?, (Some(Role::WorkspaceAdmin), vec![RoleSource::WorkspaceOwnerTeam]));
    assert_eq!(w.on_document(w.editor).await?.0, Some(Role::Editor));
    assert_eq!(w.on_document(w.other_team_member).await?.0, None);
    let mut tx = w.tx().await?;
    let on_workspace = resolve_workspace_access(&mut tx, w.lead, w.workspace).await.map_err(err)?.ok_or("workspace")?;
    assert_eq!(on_workspace.role, Some(Role::WorkspaceAdmin));
    set_owning_team(&mut tx, OwnedResource::Workspace(w.workspace), None, w.admin).await.map_err(err)?;
    tx.commit().await?;
    assert_eq!(w.on_document(w.lead).await?.0, None, "ownership withdrawn");
    Ok(())
}

#[tokio::test]
async fn a_project_shared_with_another_team_gives_that_team_the_granted_role_at_most() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let w = world(pool).await?;
    let mut tx = w.tx().await?;
    set_owning_team(&mut tx, OwnedResource::Project(w.project.as_uuid()), Some(w.team), w.admin).await.map_err(err)?;
    grant_to_team(&mut tx, w.other_team, TeamGrant::Project(w.project.as_uuid(), WorkspaceGrantRole::Commenter), w.admin).await.map_err(err)?;
    tx.commit().await?;
    assert_eq!(w.on_document(w.other_team_member).await?, (Some(Role::Commenter), vec![RoleSource::ProjectTeamGrant]));

    // A raised grant is still capped by the member's team role; a revoked one is gone.
    let mut tx = w.tx().await?;
    grant_to_team(&mut tx, w.other_team, TeamGrant::Project(w.project.as_uuid(), WorkspaceGrantRole::WorkspaceAdmin), w.admin).await.map_err(err)?;
    let listed = list_project_team_grants(&mut tx, w.project.as_uuid()).await.map_err(err)?;
    assert_eq!(listed.len(), 1);
    assert_eq!((listed[0].team_id, listed[0].role.as_str()), (w.other_team, "workspace_admin"));
    tx.commit().await?;
    assert_eq!(w.on_document(w.other_team_member).await?.0, Some(Role::Editor), "an editor member of the team is capped at editor");
    let mut tx = w.tx().await?;
    assert!(revoke_team_grant(&mut tx, w.other_team, GrantTarget::Project(w.project.as_uuid()), w.admin).await.map_err(err)?);
    assert!(
        !revoke_team_grant(&mut tx, w.other_team, GrantTarget::Project(w.project.as_uuid()), w.admin).await.map_err(err)?,
        "nothing left to revoke"
    );
    tx.commit().await?;
    assert_eq!(w.on_document(w.other_team_member).await?.0, None);
    // Another workspace's project is not reachable through this grant.
    let mut tx = w.tx().await?;
    assert!(
        grant_to_team(&mut tx, w.other_team, TeamGrant::Project(Uuid::new_v4(), WorkspaceGrantRole::Viewer), w.admin).await.is_err(),
        "unknown project"
    );
    let _ = tx.rollback().await;
    Ok(())
}

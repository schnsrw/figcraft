//! Memberships, team grants, sensitive permissions and effective-role resolution against the dev
//! PostgreSQL (fixture prefix c4; skipped without FIGCRAFT_TEST_DATABASE_URL_FILE). Every test
//! builds a fresh tenant whose ids all start with `c4`, so reruns never collide.

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, ProjectId, ResourceState, Role, TenantId, Uuid, WorkspaceId};
use figcraft_cloud_storage::effective_roles::{
    RoleSource, resolve_document_access, resolve_project_access, resolve_team_role, resolve_tenant_access, resolve_workspace_access,
};
use figcraft_cloud_storage::memberships::{
    MemberPermission, MemberState, MembershipError, add_tenant_member, change_tenant_member_role, get_tenant_member, grant_member_permission,
    list_member_permissions, list_tenant_members, reactivate_tenant_member, remove_document_member, remove_tenant_member,
    remove_workspace_team_grant, revoke_member_permission, set_document_member, set_document_team_grant, set_workspace_member,
    set_workspace_team_grant, suspend_tenant_member,
};
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use sqlx::postgres::{PgPool, PgPoolOptions};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// A random UUID whose first byte is 0xc4 (fixture prefix c4).
fn c4() -> Uuid {
    let mut bytes = *Uuid::new_v4().as_bytes();
    bytes[0] = 0xc4;
    Uuid::from_bytes(bytes)
}

async fn pool() -> Result<Option<PgPool>, Box<dyn std::error::Error>> {
    let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; membership probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    Ok(Some(PgPoolOptions::new().max_connections(4).connect(database_url.trim_end()).await?))
}

/// A fresh tenant with one workspace, project and document, and an active owner.
struct World {
    pool: PgPool,
    tenant: TenantId,
    owner: PrincipalId,
    workspace: WorkspaceId,
    project: ProjectId,
    document: CloudDocumentId,
}

impl World {
    async fn new(pool: PgPool) -> Result<Self, Box<dyn std::error::Error>> {
        let tenant_uuid = c4();
        let tenant: TenantId = tenant_uuid.to_string().parse()?;
        let slug = format!("c4-{}", &tenant_uuid.simple().to_string()[..16]);
        let world = Self {
            pool,
            tenant,
            owner: c4().to_string().parse()?,
            workspace: c4().to_string().parse()?,
            project: c4().to_string().parse()?,
            document: c4().to_string().parse()?,
        };
        let mut transaction = world.tx().await?;
        sqlx::query("INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $2, 'c4 memberships', 'test', 'local')")
            .bind(world.tenant.to_string())
            .bind(slug)
            .execute(transaction.connection())
            .await?;
        sqlx::query("INSERT INTO workspaces (id, tenant_id, name) VALUES ($1::uuid, $2::uuid, 'c4 workspace')")
            .bind(world.workspace.to_string())
            .bind(world.tenant.to_string())
            .execute(transaction.connection())
            .await?;
        sqlx::query("INSERT INTO projects (id, tenant_id, workspace_id, name) VALUES ($1::uuid, $2::uuid, $3::uuid, 'c4 project')")
            .bind(world.project.to_string())
            .bind(world.tenant.to_string())
            .bind(world.workspace.to_string())
            .execute(transaction.connection())
            .await?;
        sqlx::query(
            "INSERT INTO documents (id, tenant_id, workspace_id, project_id, name) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 'c4 doc')",
        )
        .bind(world.document.to_string())
        .bind(world.tenant.to_string())
        .bind(world.workspace.to_string())
        .bind(world.project.to_string())
        .execute(transaction.connection())
        .await?;
        transaction.commit().await?;
        world.principal_in(world.owner, Role::TenantOwner).await?;
        Ok(world)
    }

    async fn tx(&self) -> Result<TenantTransaction<'_>, Box<dyn std::error::Error>> {
        Ok(begin_tenant_transaction(&self.pool, self.tenant).await.map_err(|error| format!("{error:?}"))?)
    }

    async fn principal(&self) -> Result<PrincipalId, Box<dyn std::error::Error>> {
        let id: PrincipalId = c4().to_string().parse()?;
        let mut transaction = self.tx().await?;
        sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'c4 member')")
            .bind(id.to_string())
            .execute(transaction.connection())
            .await?;
        transaction.commit().await?;
        Ok(id)
    }

    async fn principal_in(&self, id: PrincipalId, role: Role) -> TestResult {
        let mut transaction = self.tx().await?;
        sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'c4 member') ON CONFLICT DO NOTHING")
            .bind(id.to_string())
            .execute(transaction.connection())
            .await?;
        add_tenant_member(&mut transaction, id, role).await?;
        transaction.commit().await?;
        Ok(())
    }

    async fn member(&self, role: Role) -> Result<PrincipalId, Box<dyn std::error::Error>> {
        let id = self.principal().await?;
        self.principal_in(id, role).await?;
        Ok(id)
    }

    /// A new active team with `members` (principal, team role).
    async fn team(&self, members: &[(PrincipalId, &str)]) -> Result<Uuid, Box<dyn std::error::Error>> {
        let team = c4();
        let mut transaction = self.tx().await?;
        sqlx::query("INSERT INTO teams (id, tenant_id, slug, name) VALUES ($1::uuid, $2::uuid, $3, 'c4 team')")
            .bind(team.to_string())
            .bind(self.tenant.to_string())
            .bind(format!("c4-{}", &team.simple().to_string()[..16]))
            .execute(transaction.connection())
            .await?;
        for (principal, role) in members {
            set_team_member(&mut transaction, team, *principal, role, "active").await?;
        }
        transaction.commit().await?;
        Ok(team)
    }

    async fn document_role(&self, principal: PrincipalId) -> Result<Option<Role>, Box<dyn std::error::Error>> {
        let mut transaction = self.tx().await?;
        let access = resolve_document_access(&mut transaction, principal, self.document).await?.ok_or("document missing")?;
        transaction.commit().await?;
        Ok(access.role)
    }

    async fn workspace_role(&self, principal: PrincipalId) -> Result<Option<Role>, Box<dyn std::error::Error>> {
        let mut transaction = self.tx().await?;
        let access = resolve_workspace_access(&mut transaction, principal, self.workspace).await?.ok_or("workspace missing")?;
        transaction.commit().await?;
        Ok(access.role)
    }

    async fn exec(&self, statement: &'static str, id: String) -> TestResult {
        let mut transaction = self.tx().await?;
        sqlx::query(statement).bind(self.tenant.to_string()).bind(id).execute(transaction.connection()).await?;
        transaction.commit().await?;
        Ok(())
    }
}

/// Team membership is owned by S4-009; the tests write it directly.
async fn set_team_member(transaction: &mut TenantTransaction<'_>, team: Uuid, principal: PrincipalId, role: &str, state: &str) -> TestResult {
    sqlx::query(
        "INSERT INTO team_members (tenant_id, team_id, principal_id, role, state) VALUES ($1::uuid, $2::uuid, $3::uuid, $4, $5) \
         ON CONFLICT (tenant_id, team_id, principal_id) DO UPDATE SET role = EXCLUDED.role, state = EXCLUDED.state",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(team.to_string())
    .bind(principal.to_string())
    .bind(role)
    .bind(state)
    .execute(transaction.connection())
    .await?;
    Ok(())
}

#[tokio::test]
async fn team_grants_are_inherited_and_direct_viewer_never_downgrades() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let world = World::new(pool).await?;
    let alice = world.member(Role::Editor).await?;
    let bob = world.member(Role::Editor).await?;

    // A plain tenant editor sees nothing until something grants access; the owner administers all.
    assert_eq!(world.document_role(alice).await?, None);
    assert_eq!(world.document_role(world.owner).await?, Some(Role::WorkspaceAdmin));

    // A workspace team grant gives editor on the workspace, its project and its document.
    let team = world.team(&[(alice, "editor"), (bob, "viewer")]).await?;
    let mut transaction = world.tx().await?;
    set_workspace_team_grant(&mut transaction, world.workspace, team, Role::Editor).await?;
    transaction.commit().await?;
    assert_eq!(world.workspace_role(alice).await?, Some(Role::Editor));
    assert_eq!(world.document_role(alice).await?, Some(Role::Editor));
    let mut transaction = world.tx().await?;
    let project = resolve_project_access(&mut transaction, alice, world.project).await?.ok_or("project missing")?;
    transaction.commit().await?;
    assert_eq!((project.role, project.state), (Some(Role::Editor), ResourceState::Active));
    // The grant is capped by the team role: a team viewer gets viewer.
    assert_eq!(world.document_role(bob).await?, Some(Role::Viewer));

    // A direct viewer membership does not downgrade the stronger team grant.
    let mut transaction = world.tx().await?;
    set_document_member(&mut transaction, world.document, alice, Role::Viewer).await?;
    set_workspace_member(&mut transaction, world.workspace, alice, Role::Viewer).await?;
    let access = resolve_document_access(&mut transaction, alice, world.document).await?.ok_or("document missing")?;
    transaction.commit().await?;
    assert_eq!(access.role, Some(Role::Editor));
    assert_eq!(access.sources, vec![RoleSource::WorkspaceTeamGrant]);

    // Removing the team membership removes the inherited access; the direct viewer row remains.
    let mut transaction = world.tx().await?;
    set_team_member(&mut transaction, team, alice, "editor", "deleted").await?;
    transaction.commit().await?;
    assert_eq!(world.document_role(alice).await?, Some(Role::Viewer));
    let mut transaction = world.tx().await?;
    assert!(remove_document_member(&mut transaction, world.document, alice).await?);
    assert!(!remove_document_member(&mut transaction, world.document, alice).await?);
    transaction.commit().await?;
    world.exec("DELETE FROM workspace_members WHERE tenant_id = $1::uuid AND principal_id = $2::uuid", alice.to_string()).await?;
    assert_eq!(world.document_role(alice).await?, None);

    // A suspended team grants nothing; revoking the grant removes it too.
    assert_eq!(world.document_role(bob).await?, Some(Role::Viewer));
    world.exec("UPDATE teams SET state = 'suspended' WHERE tenant_id = $1::uuid AND id = $2::uuid", team.to_string()).await?;
    assert_eq!(world.document_role(bob).await?, None);
    world.exec("UPDATE teams SET state = 'active' WHERE tenant_id = $1::uuid AND id = $2::uuid", team.to_string()).await?;
    let mut transaction = world.tx().await?;
    assert!(remove_workspace_team_grant(&mut transaction, world.workspace, team).await?);
    transaction.commit().await?;
    assert_eq!(world.document_role(bob).await?, None);

    // A document team grant applies to that document only.
    let mut transaction = world.tx().await?;
    set_document_team_grant(&mut transaction, world.document, team, Role::Commenter).await?;
    transaction.commit().await?;
    assert_eq!(world.document_role(bob).await?, Some(Role::Viewer));
    assert_eq!(world.workspace_role(bob).await?, None);
    let mut transaction = world.tx().await?;
    let (state, role) = resolve_team_role(&mut transaction, bob, team).await?.ok_or("team missing")?;
    transaction.commit().await?;
    assert_eq!((state, role), (ResourceState::Active, Some(Role::Viewer)));
    Ok(())
}

#[tokio::test]
async fn guests_are_capped_and_role_boundaries_are_enforced() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let world = World::new(pool).await?;
    let guest = world.member(Role::Viewer).await?;
    let outsider = world.principal().await?;
    let mut transaction = world.tx().await?;
    set_document_member(&mut transaction, world.document, guest, Role::Editor).await?;
    assert_eq!(set_document_member(&mut transaction, world.document, guest, Role::WorkspaceAdmin).await, Err(MembershipError::InvalidRole));
    assert_eq!(set_workspace_member(&mut transaction, world.workspace, guest, Role::TenantOwner).await, Err(MembershipError::InvalidRole));
    assert_eq!(set_document_member(&mut transaction, world.document, outsider, Role::Viewer).await, Err(MembershipError::NotActiveMember));
    assert_eq!(set_document_member(&mut transaction, CloudDocumentId::random(), guest, Role::Viewer).await, Err(MembershipError::NotFound));
    assert_eq!(set_workspace_team_grant(&mut transaction, world.workspace, c4(), Role::Viewer).await, Err(MembershipError::NotFound));
    assert_eq!(add_tenant_member(&mut transaction, outsider, Role::TeamManager).await, Err(MembershipError::InvalidRole));
    transaction.commit().await?;
    // A tenant viewer stays a viewer even with a direct editor membership.
    assert_eq!(world.document_role(guest).await?, Some(Role::Viewer));

    // Archived workspaces propagate their state down to documents.
    world.exec("UPDATE workspaces SET state = 'archived' WHERE tenant_id = $1::uuid AND id = $2::uuid", world.workspace.to_string()).await?;
    let mut transaction = world.tx().await?;
    let access = resolve_document_access(&mut transaction, guest, world.document).await?.ok_or("document missing")?;
    assert!(resolve_document_access(&mut transaction, guest, CloudDocumentId::random()).await?.is_none());
    transaction.commit().await?;
    assert_eq!(access.state, ResourceState::Archived);
    Ok(())
}

#[tokio::test]
async fn suspended_tenants_members_and_principals_get_nothing() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let world = World::new(pool).await?;
    let alice = world.member(Role::Editor).await?;
    let mut transaction = world.tx().await?;
    set_document_member(&mut transaction, world.document, alice, Role::Editor).await?;
    grant_member_permission(&mut transaction, alice, MemberPermission::Export, world.owner).await?;
    let access = resolve_tenant_access(&mut transaction, alice).await?.ok_or("tenant missing")?;
    transaction.commit().await?;
    assert_eq!(access.tenant_role, Some(Role::Editor));
    assert_eq!(access.permissions, vec![MemberPermission::Export]);
    assert_eq!(world.document_role(alice).await?, Some(Role::Editor));

    // Suspended member: grants kept, but nothing resolves; reactivation restores them.
    let mut transaction = world.tx().await?;
    suspend_tenant_member(&mut transaction, alice).await?;
    let access = resolve_tenant_access(&mut transaction, alice).await?.ok_or("tenant missing")?;
    transaction.commit().await?;
    assert_eq!((access.tenant_role, access.permissions.len()), (None, 0));
    assert_eq!(world.document_role(alice).await?, None);
    let mut transaction = world.tx().await?;
    assert_eq!(set_document_member(&mut transaction, world.document, alice, Role::Viewer).await, Err(MembershipError::NotActiveMember));
    reactivate_tenant_member(&mut transaction, alice).await?;
    transaction.commit().await?;
    assert_eq!(world.document_role(alice).await?, Some(Role::Editor));

    // Suspended principal.
    world.exec("UPDATE principals SET state = 'suspended' WHERE $1::uuid IS NOT NULL AND id = $2::uuid", alice.to_string()).await?;
    assert_eq!(world.document_role(alice).await?, None);
    world.exec("UPDATE principals SET state = 'active' WHERE $1::uuid IS NOT NULL AND id = $2::uuid", alice.to_string()).await?;

    // Suspended tenant: nobody, not even the owner, resolves a role or permission.
    world.exec("UPDATE tenants SET state = 'suspended' WHERE id = $1::uuid AND id = $2::uuid", world.tenant.to_string()).await?;
    for principal in [alice, world.owner] {
        let mut transaction = world.tx().await?;
        let access = resolve_document_access(&mut transaction, principal, world.document).await?.ok_or("document missing")?;
        transaction.commit().await?;
        assert_eq!(access.tenant.tenant_state, ResourceState::Suspended);
        assert_eq!((access.tenant.tenant_role, access.role, access.tenant.permissions.len()), (None, None, 0));
    }
    world.exec("UPDATE tenants SET state = 'active' WHERE id = $1::uuid AND id = $2::uuid", world.tenant.to_string()).await?;
    assert_eq!(world.document_role(alice).await?, Some(Role::Editor));
    Ok(())
}

#[tokio::test]
async fn the_last_owner_cannot_be_demoted_suspended_or_removed() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let world = World::new(pool).await?;
    let mut transaction = world.tx().await?;
    assert_eq!(change_tenant_member_role(&mut transaction, world.owner, Role::TenantAdmin).await, Err(MembershipError::LastOwner));
    assert_eq!(suspend_tenant_member(&mut transaction, world.owner).await, Err(MembershipError::LastOwner));
    assert_eq!(remove_tenant_member(&mut transaction, world.owner).await, Err(MembershipError::LastOwner));
    assert_eq!(add_tenant_member(&mut transaction, world.owner, Role::Editor).await, Err(MembershipError::Conflict));
    transaction.commit().await?;

    // With a second active owner the first can step down; the second is then the last.
    let second = world.member(Role::TenantOwner).await?;
    let alice = world.member(Role::Editor).await?;
    let team = world.team(&[(alice, "editor")]).await?;
    let mut transaction = world.tx().await?;
    set_workspace_member(&mut transaction, world.workspace, alice, Role::Editor).await?;
    grant_member_permission(&mut transaction, alice, MemberPermission::Billing, world.owner).await?;
    change_tenant_member_role(&mut transaction, world.owner, Role::TenantAdmin).await?;
    assert_eq!(remove_tenant_member(&mut transaction, second).await, Err(MembershipError::LastOwner));
    change_tenant_member_role(&mut transaction, world.owner, Role::TenantOwner).await?;
    remove_tenant_member(&mut transaction, second).await?;
    assert_eq!(remove_tenant_member(&mut transaction, second).await, Err(MembershipError::NotFound));
    // Removing a member drops their memberships, team memberships and permissions.
    remove_tenant_member(&mut transaction, alice).await?;
    let member = get_tenant_member(&mut transaction, alice).await?.ok_or("member missing")?;
    assert_eq!(member.state, MemberState::Deleted);
    assert!(list_member_permissions(&mut transaction, alice).await?.is_empty());
    let (_, team_role) = resolve_team_role(&mut transaction, alice, team).await?.ok_or("team missing")?;
    assert_eq!(team_role, None);
    let listed = list_tenant_members(&mut transaction, None).await?;
    assert_eq!(listed.iter().map(|member| member.principal_id).collect::<Vec<_>>(), vec![world.owner]);
    transaction.commit().await?;
    assert_eq!(world.document_role(alice).await?, None);

    // A removed member can be re-added with a new role.
    let mut transaction = world.tx().await?;
    add_tenant_member(&mut transaction, alice, Role::Commenter).await?;
    let member = get_tenant_member(&mut transaction, alice).await?.ok_or("member missing")?;
    transaction.commit().await?;
    assert_eq!((member.role, member.state), (Role::Commenter, MemberState::Active));
    Ok(())
}

#[tokio::test]
async fn sensitive_permissions_are_explicit_rows_only() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let world = World::new(pool).await?;
    let outsider = world.principal().await?;
    let mut transaction = world.tx().await?;
    // The owner holds no sensitive permission until one is granted explicitly.
    let access = resolve_tenant_access(&mut transaction, world.owner).await?.ok_or("tenant missing")?;
    assert_eq!(access.tenant_role, Some(Role::TenantOwner));
    assert!(access.permissions.is_empty());
    for permission in MemberPermission::ALL {
        assert!(grant_member_permission(&mut transaction, world.owner, permission, world.owner).await?);
        assert!(!grant_member_permission(&mut transaction, world.owner, permission, world.owner).await?);
    }
    assert_eq!(list_member_permissions(&mut transaction, world.owner).await?.len(), MemberPermission::ALL.len());
    assert!(revoke_member_permission(&mut transaction, world.owner, MemberPermission::ProviderKeys).await?);
    assert!(!revoke_member_permission(&mut transaction, world.owner, MemberPermission::ProviderKeys).await?);
    let access = resolve_tenant_access(&mut transaction, world.owner).await?.ok_or("tenant missing")?;
    assert!(!access.permissions.contains(&MemberPermission::ProviderKeys));
    assert_eq!(access.permissions.len(), MemberPermission::ALL.len() - 1);
    assert_eq!(
        grant_member_permission(&mut transaction, outsider, MemberPermission::Billing, world.owner).await,
        Err(MembershipError::NotActiveMember)
    );
    transaction.commit().await?;
    Ok(())
}

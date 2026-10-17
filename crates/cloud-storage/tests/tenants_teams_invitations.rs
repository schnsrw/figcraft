//! S4-007 tenants and S4-009 teams/invitations against the dev PostgreSQL (fixture prefix c5;
//! skipped without FIGCRAFT_TEST_DATABASE_URL_FILE). Every run creates fresh c5-prefixed ids and
//! slugs, so reruns pass single-threaded or in parallel.

use figcraft_cloud_domain::{PrincipalId, ProjectId, ResourceState, TenantId, Uuid, WorkspaceId};
use figcraft_cloud_storage::invitations::{
    InvitationError, TOKEN_CHARS, accept_invitation, create_invitation, invitation_token_hash, list_pending_invitations, revoke_invitation,
};
use figcraft_cloud_storage::metadata::{PageSize, create_project, create_workspace};
use figcraft_cloud_storage::teams::{
    GrantTarget, LibraryGrantRole, OwnedResource, TeamError, TeamGrant, TeamRole, WorkspaceGrantRole, add_team_member, archive_team,
    change_team_member_role, create_team, grant_to_team, list_team_members, list_teams, remove_team_member, rename_team, revoke_team_grant,
    set_owning_team,
};
use figcraft_cloud_storage::tenants::{
    NewTenant, TenantError, assign_plan, create_tenant_with_owner, list_tenants_for_principal, reactivate_tenant, suspend_tenant,
};
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use sqlx::postgres::{PgPool, PgPoolOptions};

/// Fixtures and raw inspection queries run as the administrator: row-level security (S4-014)
/// hides every tenant row from the runtime role outside a tenant transaction. Repository calls
/// still run as the runtime role.
async fn admin_pool() -> Result<sqlx::PgPool, sqlx::Error> {
    let file = std::env::var("FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE")
        .map_err(|_| sqlx::Error::Configuration("FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE is unset".into()))?;
    let url = std::fs::read_to_string(file)?;
    sqlx::postgres::PgPoolOptions::new().max_connections(1).connect(url.trim_end()).await
}

type TestResult = Result<(), Box<dyn std::error::Error>>;
type Fallible<T> = Result<T, Box<dyn std::error::Error>>;

/// A random UUIDv4 whose first byte is the c5 fixture prefix.
fn c5() -> Uuid {
    let mut bytes = *Uuid::new_v4().as_bytes();
    bytes[0] = 0xc5;
    Uuid::from_bytes(bytes)
}

fn suffix() -> String {
    c5().simple().to_string()[2..14].to_owned()
}

async fn pool() -> Fallible<Option<PgPool>> {
    let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; tenant/team/invitation probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    Ok(Some(PgPoolOptions::new().max_connections(6).connect(database_url.trim_end()).await?))
}

async fn tx(pool: &PgPool, tenant: TenantId) -> Fallible<TenantTransaction<'_>> {
    begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("{error:?}").into())
}

async fn principal(_pool: &PgPool) -> Fallible<PrincipalId> {
    let id = c5();
    sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'c5 probe')")
        .bind(id.to_string())
        .execute(&admin_pool().await?)
        .await?;
    Ok(PrincipalId::try_from(id).map_err(|_| "nil")?)
}

/// A fresh active tenant owned by a fresh principal.
async fn tenant(pool: &PgPool) -> Fallible<(TenantId, PrincipalId)> {
    let owner = principal(pool).await?;
    let id = TenantId::try_from(c5()).map_err(|_| "nil")?;
    let slug = format!("c5-{}", suffix());
    let mut transaction = tx(pool, id).await?;
    create_tenant_with_owner(&mut transaction, NewTenant { slug: &slug, name: "c5 tenant", plan_id: "team", home_region: "local" }, owner).await?;
    transaction.commit().await?;
    Ok((id, owner))
}

async fn add_tenant_member(_pool: &PgPool, tenant: TenantId, member: PrincipalId, role: &str) -> TestResult {
    sqlx::query("INSERT INTO tenant_members (tenant_id, principal_id, role) VALUES ($1::uuid, $2::uuid, $3)")
        .bind(tenant.to_string())
        .bind(member.to_string())
        .bind(role)
        .execute(&admin_pool().await?)
        .await?;
    Ok(())
}

async fn audit_count(_pool: &PgPool, tenant: TenantId, action: &str) -> Fallible<i64> {
    Ok(sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE tenant_id = $1::uuid AND action = $2")
        .bind(tenant.to_string())
        .bind(action)
        .fetch_one(&admin_pool().await?)
        .await?)
}

async fn tenant_role(_pool: &PgPool, tenant: TenantId, member: PrincipalId) -> Fallible<Option<(String, String)>> {
    Ok(sqlx::query_as("SELECT role, state FROM tenant_members WHERE tenant_id = $1::uuid AND principal_id = $2::uuid")
        .bind(tenant.to_string())
        .bind(member.to_string())
        .fetch_optional(&admin_pool().await?)
        .await?)
}

async fn new_team(pool: &PgPool, tenant: TenantId, manager: PrincipalId) -> Fallible<Uuid> {
    let id = c5();
    let mut transaction = tx(pool, tenant).await?;
    create_team(&mut transaction, id, &format!("team-{}", suffix()), "c5 team", manager).await?;
    transaction.commit().await?;
    Ok(id)
}

#[tokio::test]
async fn tenants_are_created_with_an_owner_and_move_through_their_lifecycle() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let (tenant_id, owner) = tenant(&pool).await?;
    assert_eq!(tenant_role(&pool, tenant_id, owner).await?, Some(("tenant_owner".into(), "active".into())));
    assert_eq!(audit_count(&pool, tenant_id, "tenant.create").await?, 1);

    // Validation happens before any write; an unusable owner rolls the tenant row back.
    let fresh = TenantId::try_from(c5()).map_err(|_| "nil")?;
    let slug = format!("c5-{}", suffix());
    for (new, expected) in [
        (NewTenant { slug: "admin", name: "x", plan_id: "team", home_region: "local" }, TenantError::InvalidSlug),
        (NewTenant { slug: "c5-bad-", name: "x", plan_id: "team", home_region: "local" }, TenantError::InvalidSlug),
        (NewTenant { slug: &slug, name: "  ", plan_id: "team", home_region: "local" }, TenantError::InvalidName),
        (NewTenant { slug: &slug, name: "x", plan_id: "unlimited", home_region: "local" }, TenantError::UnknownPlan),
        (NewTenant { slug: &slug, name: "x", plan_id: "team", home_region: "EU West" }, TenantError::InvalidRegion),
    ] {
        let mut transaction = tx(&pool, fresh).await?;
        assert_eq!(create_tenant_with_owner(&mut transaction, new, owner).await, Err(expected));
    }
    let mut transaction = tx(&pool, fresh).await?;
    let ghost = PrincipalId::try_from(c5()).map_err(|_| "nil")?;
    let new = NewTenant { slug: &slug, name: "c5", plan_id: "team", home_region: "local" };
    assert_eq!(create_tenant_with_owner(&mut transaction, new, ghost).await, Err(TenantError::OwnerUnavailable));
    drop(transaction);
    let existing: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tenants WHERE id = $1::uuid").bind(fresh.to_string()).fetch_one(&admin_pool().await?).await?;
    assert_eq!(existing, 0);
    // A taken slug is a conflict.
    let taken: String =
        sqlx::query_scalar("SELECT slug FROM tenants WHERE id = $1::uuid").bind(tenant_id.to_string()).fetch_one(&admin_pool().await?).await?;
    let mut transaction = tx(&pool, fresh).await?;
    let new = NewTenant { slug: &taken, name: "c5", plan_id: "team", home_region: "local" };
    assert_eq!(create_tenant_with_owner(&mut transaction, new, owner).await, Err(TenantError::Conflict));
    drop(transaction);

    // Suspend (idempotent), reactivate (idempotent), assign plans.
    let mut transaction = tx(&pool, tenant_id).await?;
    assert_eq!(suspend_tenant(&mut transaction, owner, Some("")).await, Err(TenantError::InvalidReason));
    assert_eq!(suspend_tenant(&mut transaction, owner, Some("payment lapsed")).await?.state, ResourceState::Suspended);
    assert_eq!(suspend_tenant(&mut transaction, owner, None).await?.state, ResourceState::Suspended);
    assert_eq!(assign_plan(&mut transaction, owner, "business").await?.plan_id, "business");
    assert_eq!(assign_plan(&mut transaction, owner, "business").await?.plan_id, "business");
    assert_eq!(assign_plan(&mut transaction, owner, "gold").await, Err(TenantError::UnknownPlan));
    assert_eq!(reactivate_tenant(&mut transaction, owner).await?.state, ResourceState::Active);
    assert_eq!(reactivate_tenant(&mut transaction, owner).await?.state, ResourceState::Active);
    transaction.commit().await?;
    assert_eq!(audit_count(&pool, tenant_id, "tenant.suspend").await?, 1);
    assert_eq!(audit_count(&pool, tenant_id, "tenant.reactivate").await?, 1);
    assert_eq!(audit_count(&pool, tenant_id, "tenant.plan_assign").await?, 1);

    // A deleting tenant cannot be suspended or reactivated.
    sqlx::query("UPDATE tenants SET state = 'deleting' WHERE id = $1::uuid").bind(tenant_id.to_string()).execute(&admin_pool().await?).await?;
    let mut transaction = tx(&pool, tenant_id).await?;
    assert_eq!(suspend_tenant(&mut transaction, owner, None).await, Err(TenantError::InvalidState));
    assert_eq!(reactivate_tenant(&mut transaction, owner).await, Err(TenantError::InvalidState));
    Ok(())
}

#[tokio::test]
async fn tenants_for_a_principal_are_listed_with_roles_in_bounded_keyset_pages() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let (first, owner) = tenant(&pool).await?;
    let (second, other_owner) = tenant(&pool).await?;
    add_tenant_member(&pool, second, owner, "editor").await?;
    let (hidden, third_owner) = tenant(&pool).await?;
    add_tenant_member(&pool, hidden, owner, "viewer").await?;
    sqlx::query("UPDATE tenant_members SET state = 'suspended' WHERE tenant_id = $1::uuid AND principal_id = $2::uuid")
        .bind(hidden.to_string())
        .bind(owner.to_string())
        .execute(&admin_pool().await?)
        .await?;
    let _ = (other_owner, third_owner);

    let one = PageSize::new(1)?;
    let page = list_tenants_for_principal(&pool, owner, None, one).await?;
    assert_eq!(page.items.len(), 1);
    let cursor = page.next_cursor.ok_or("expected a second page")?;
    let rest = list_tenants_for_principal(&pool, owner, Some(cursor), PageSize::new(100)?).await?;
    assert_eq!(rest.next_cursor, None);
    let mut seen: Vec<(TenantId, String)> = page.items.into_iter().chain(rest.items).map(|item| (item.tenant.id, item.role)).collect();
    seen.sort();
    let mut expected = vec![(first, "tenant_owner".to_owned()), (second, "editor".to_owned())];
    expected.sort();
    assert_eq!(seen, expected);
    assert!(PageSize::new(101).is_err());
    Ok(())
}

#[tokio::test]
async fn teams_keep_a_manager_and_own_or_hold_grants_on_resources() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let (tenant_id, owner) = tenant(&pool).await?;
    let member = principal(&pool).await?;
    add_tenant_member(&pool, tenant_id, member, "editor").await?;
    let outsider = principal(&pool).await?;
    let team = new_team(&pool, tenant_id, owner).await?;

    let mut transaction = tx(&pool, tenant_id).await?;
    assert_eq!(create_team(&mut transaction, c5(), "Bad Slug", "x", owner).await, Err(TeamError::InvalidSlug));
    assert_eq!(create_team(&mut transaction, c5(), "ok", "x", outsider).await, Err(TeamError::NotTenantMember));
    transaction.rollback().await?;

    let mut transaction = tx(&pool, tenant_id).await?;
    assert_eq!(add_team_member(&mut transaction, team, outsider, TeamRole::Editor, owner).await, Err(TeamError::NotTenantMember));
    add_team_member(&mut transaction, team, member, TeamRole::Editor, owner).await?;
    assert_eq!(add_team_member(&mut transaction, team, member, TeamRole::Viewer, owner).await, Err(TeamError::Conflict));
    // The only manager can be neither demoted nor removed.
    assert_eq!(change_team_member_role(&mut transaction, team, owner, TeamRole::Editor, owner).await, Err(TeamError::LastManager));
    assert_eq!(remove_team_member(&mut transaction, team, owner, owner).await, Err(TeamError::LastManager));
    // Once another manager exists, the first can leave.
    change_team_member_role(&mut transaction, team, member, TeamRole::TeamManager, owner).await?;
    assert!(remove_team_member(&mut transaction, team, owner, owner).await?);
    assert!(!remove_team_member(&mut transaction, team, owner, owner).await?);
    assert_eq!(remove_team_member(&mut transaction, team, member, member).await, Err(TeamError::LastManager));
    // A removed member can be re-added.
    add_team_member(&mut transaction, team, owner, TeamRole::Viewer, member).await?;
    let members = list_team_members(&mut transaction, team, None, PageSize::new(10)?).await?;
    let mut roles: Vec<(PrincipalId, TeamRole)> = members.items.into_iter().map(|m| (m.principal_id, m.role)).collect();
    roles.sort_by_key(|(id, _)| *id);
    let mut expected = vec![(owner, TeamRole::Viewer), (member, TeamRole::TeamManager)];
    expected.sort_by_key(|(id, _)| *id);
    assert_eq!(roles, expected);
    assert_eq!(rename_team(&mut transaction, team, "c5 renamed", member).await?.name, "c5 renamed");
    assert_eq!(list_teams(&mut transaction, None, PageSize::new(10)?).await?.items.len(), 1);

    // Ownership and grants on a workspace, project and library.
    let workspace = WorkspaceId::random();
    create_workspace(&mut transaction, workspace, "c5 workspace", None).await?;
    let project = ProjectId::random();
    create_project(&mut transaction, project, workspace, "c5 project", None).await?;
    let library = c5();
    sqlx::query("INSERT INTO libraries (id, tenant_id, name) VALUES ($1::uuid, $2::uuid, 'c5 library')")
        .bind(library.to_string())
        .bind(tenant_id.to_string())
        .execute(transaction.connection())
        .await?;
    set_owning_team(&mut transaction, OwnedResource::Workspace(workspace), Some(team), member).await?;
    set_owning_team(&mut transaction, OwnedResource::Project(project.as_uuid()), Some(team), member).await?;
    set_owning_team(&mut transaction, OwnedResource::Library(library), Some(team), member).await?;
    set_owning_team(&mut transaction, OwnedResource::Library(library), Some(team), member).await?;
    assert_eq!(set_owning_team(&mut transaction, OwnedResource::Project(c5()), Some(team), member).await, Err(TeamError::ResourceNotFound));
    let owners: (Option<String>, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT (SELECT owning_team_id::text FROM workspaces WHERE id = $1::uuid), (SELECT owning_team_id::text FROM projects WHERE id = $2::uuid), \
                (SELECT owning_team_id::text FROM libraries WHERE id = $3::uuid)",
    )
    .bind(workspace.to_string())
    .bind(project.to_string())
    .bind(library.to_string())
    .fetch_one(transaction.connection())
    .await?;
    let team_text = Some(team.to_string());
    assert_eq!(owners, (team_text.clone(), team_text.clone(), team_text));
    set_owning_team(&mut transaction, OwnedResource::Project(project.as_uuid()), None, member).await?;
    grant_to_team(&mut transaction, team, TeamGrant::Workspace(workspace, WorkspaceGrantRole::Editor), member).await?;
    grant_to_team(&mut transaction, team, TeamGrant::Workspace(workspace, WorkspaceGrantRole::Viewer), member).await?;
    grant_to_team(&mut transaction, team, TeamGrant::Library(library, LibraryGrantRole::Publisher), member).await?;
    let grant: String = sqlx::query_scalar("SELECT role FROM workspace_team_grants WHERE workspace_id = $1::uuid AND team_id = $2::uuid")
        .bind(workspace.to_string())
        .bind(team.to_string())
        .fetch_one(transaction.connection())
        .await?;
    assert_eq!(grant, "viewer");
    assert!(revoke_team_grant(&mut transaction, team, GrantTarget::Library(library), member).await?);
    assert!(!revoke_team_grant(&mut transaction, team, GrantTarget::Library(library), member).await?);

    // Archived teams take no new members, owners or grants, but grants can still be revoked.
    assert_eq!(archive_team(&mut transaction, team, member).await?.state, ResourceState::Archived);
    assert_eq!(archive_team(&mut transaction, team, member).await?.state, ResourceState::Archived);
    assert_eq!(add_team_member(&mut transaction, team, outsider, TeamRole::Viewer, member).await, Err(TeamError::InvalidState));
    assert_eq!(set_owning_team(&mut transaction, OwnedResource::Workspace(workspace), Some(team), member).await, Err(TeamError::InvalidState));
    assert!(revoke_team_grant(&mut transaction, team, GrantTarget::Workspace(workspace), member).await?);
    transaction.commit().await?;

    for (action, count) in [
        ("team.create", 1),
        ("team.member_add", 2),
        ("team.member_role", 1),
        ("team.member_remove", 1),
        ("team.rename", 1),
        ("team.archive", 1),
        ("team.ownership_set", 3),
        ("team.ownership_clear", 1),
        ("team.grant_set", 3),
        ("team.grant_revoke", 2),
    ] {
        assert_eq!(audit_count(&pool, tenant_id, action).await?, count, "{action}");
    }

    // Another tenant cannot reach this team.
    let (other, other_owner) = tenant(&pool).await?;
    let mut transaction = tx(&pool, other).await?;
    assert_eq!(add_team_member(&mut transaction, team, other_owner, TeamRole::Viewer, other_owner).await, Err(TeamError::NotFound));
    Ok(())
}

#[tokio::test]
async fn invitations_are_shown_once_accepted_atomically_and_refused_when_spent() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let (tenant_id, owner) = tenant(&pool).await?;
    let team = new_team(&pool, tenant_id, owner).await?;
    let invitee = principal(&pool).await?;

    let mut transaction = tx(&pool, tenant_id).await?;
    assert_eq!(create_invitation(&mut transaction, team, "not-an-email", TeamRole::Editor, owner, None).await, Err(InvitationError::InvalidEmail));
    assert_eq!(
        create_invitation(&mut transaction, team, "a@example.com", TeamRole::Editor, owner, Some(31 * 24 * 3600)).await,
        Err(InvitationError::InvalidLifetime)
    );
    assert_eq!(
        create_invitation(&mut transaction, c5(), "a@example.com", TeamRole::Editor, owner, None).await,
        Err(InvitationError::TeamUnavailable)
    );
    let created = create_invitation(&mut transaction, team, " Ada@Example.com ", TeamRole::Editor, owner, None).await?;
    transaction.commit().await?;
    assert_eq!(created.token.len(), TOKEN_CHARS);
    assert!(!format!("{created:?}").contains(&created.token));
    let stored: (Vec<u8>, String, f64) =
        sqlx::query_as("SELECT token_hash, email, extract(epoch FROM expires_at - created_at)::float8 FROM team_invitations WHERE id = $1::uuid")
            .bind(created.id.to_string())
            .fetch_one(&admin_pool().await?)
            .await?;
    assert_eq!(stored.0, invitation_token_hash(&created.token).to_vec());
    assert_eq!(stored.1, "ada@example.com");
    assert!((stored.2 - 7.0 * 24.0 * 3600.0).abs() < 5.0);

    let mut transaction = tx(&pool, tenant_id).await?;
    let pending = list_pending_invitations(&mut transaction, Some(team), None, PageSize::new(10)?).await?;
    assert_eq!(pending.items.len(), 1);
    assert_eq!(pending.items.first().map(|item| (item.id, item.role)), Some((created.id, TeamRole::Editor)));
    drop(transaction);

    // Malformed, unknown and mismatched-email tokens change nothing.
    assert_eq!(accept_invitation(&pool, "short", invitee, None).await, Err(InvitationError::Refused));
    assert_eq!(accept_invitation(&pool, &"A".repeat(TOKEN_CHARS), invitee, None).await, Err(InvitationError::Refused));
    assert_eq!(accept_invitation(&pool, &created.token, invitee, Some("eve@example.com")).await, Err(InvitationError::EmailMismatch));
    assert_eq!(tenant_role(&pool, tenant_id, invitee).await?, None);

    let accepted = accept_invitation(&pool, &created.token, invitee, Some("ADA@example.com")).await?;
    assert_eq!((accepted.id, accepted.tenant_id, accepted.team_id, accepted.team_role), (created.id, tenant_id, team, TeamRole::Editor));
    assert!(accepted.joined_tenant);
    assert_eq!(tenant_role(&pool, tenant_id, invitee).await?, Some(("viewer".into(), "active".into())));
    let team_role: String =
        sqlx::query_scalar("SELECT role FROM team_members WHERE team_id = $1::uuid AND principal_id = $2::uuid AND state = 'active'")
            .bind(team.to_string())
            .bind(invitee.to_string())
            .fetch_one(&admin_pool().await?)
            .await?;
    assert_eq!(team_role, "editor");
    assert_eq!(accept_invitation(&pool, &created.token, invitee, None).await, Err(InvitationError::Refused));

    // An existing tenant member keeps their tenant role when accepting.
    let mut transaction = tx(&pool, tenant_id).await?;
    let second = create_invitation(&mut transaction, team, "owner@example.com", TeamRole::Viewer, owner, None).await?;
    transaction.commit().await?;
    let other_team = new_team(&pool, tenant_id, owner).await?;
    let mut transaction = tx(&pool, tenant_id).await?;
    let third = create_invitation(&mut transaction, other_team, "ada@example.com", TeamRole::Commenter, owner, None).await?;
    transaction.commit().await?;
    let again = accept_invitation(&pool, &third.token, invitee, None).await?;
    assert!(!again.joined_tenant);
    assert_eq!(tenant_role(&pool, tenant_id, invitee).await?, Some(("viewer".into(), "active".into())));

    // Revoked and expired invitations are refused.
    let mut transaction = tx(&pool, tenant_id).await?;
    assert!(revoke_invitation(&mut transaction, second.id, owner).await?);
    assert!(!revoke_invitation(&mut transaction, second.id, owner).await?);
    assert!(!revoke_invitation(&mut transaction, created.id, owner).await?);
    let expired = create_invitation(&mut transaction, team, "late@example.com", TeamRole::Viewer, owner, Some(60)).await?;
    transaction.commit().await?;
    sqlx::query("UPDATE team_invitations SET created_at = now() - interval '8 days', expires_at = now() - interval '1 day' WHERE id = $1::uuid")
        .bind(expired.id.to_string())
        .execute(&admin_pool().await?)
        .await?;
    let late = principal(&pool).await?;
    assert_eq!(accept_invitation(&pool, &second.token, late, None).await, Err(InvitationError::Refused));
    assert_eq!(accept_invitation(&pool, &expired.token, late, None).await, Err(InvitationError::Refused));
    assert_eq!(tenant_role(&pool, tenant_id, late).await?, None);
    let mut transaction = tx(&pool, tenant_id).await?;
    assert!(list_pending_invitations(&mut transaction, Some(team), None, PageSize::new(10)?).await?.items.is_empty());
    drop(transaction);

    assert_eq!(audit_count(&pool, tenant_id, "invitation.create").await?, 4);
    assert_eq!(audit_count(&pool, tenant_id, "invitation.accept").await?, 2);
    assert_eq!(audit_count(&pool, tenant_id, "invitation.revoke").await?, 1);
    Ok(())
}

#[tokio::test]
async fn a_suspended_tenant_refuses_invitation_acceptance() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let (tenant_id, owner) = tenant(&pool).await?;
    let team = new_team(&pool, tenant_id, owner).await?;
    let mut transaction = tx(&pool, tenant_id).await?;
    let created = create_invitation(&mut transaction, team, "ada@example.com", TeamRole::Editor, owner, None).await?;
    suspend_tenant(&mut transaction, owner, Some("c5 probe")).await?;
    assert_eq!(
        create_invitation(&mut transaction, team, "bob@example.com", TeamRole::Editor, owner, None).await,
        Err(InvitationError::TenantInactive)
    );
    transaction.commit().await?;

    let invitee = principal(&pool).await?;
    assert_eq!(accept_invitation(&pool, &created.token, invitee, None).await, Err(InvitationError::TenantInactive));
    assert_eq!(tenant_role(&pool, tenant_id, invitee).await?, None);
    let accepted: Option<String> = sqlx::query_scalar("SELECT accepted_at::text FROM team_invitations WHERE id = $1::uuid")
        .bind(created.id.to_string())
        .fetch_one(&admin_pool().await?)
        .await?;
    assert_eq!(accepted, None);

    // After reactivation the same invitation still works.
    let mut transaction = tx(&pool, tenant_id).await?;
    reactivate_tenant(&mut transaction, owner).await?;
    transaction.commit().await?;
    accept_invitation(&pool, &created.token, invitee, None).await?;
    Ok(())
}

#[tokio::test]
async fn concurrent_accepts_of_one_invitation_have_exactly_one_winner() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    for _round in 0..3 {
        let (tenant_id, owner) = tenant(&pool).await?;
        let team = new_team(&pool, tenant_id, owner).await?;
        let mut transaction = tx(&pool, tenant_id).await?;
        let created = create_invitation(&mut transaction, team, "race@example.com", TeamRole::Editor, owner, None).await?;
        transaction.commit().await?;
        let mut contenders = Vec::new();
        for _ in 0..4 {
            contenders.push(principal(&pool).await?);
        }
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(contenders.len()));
        let handles: Vec<_> = contenders
            .iter()
            .map(|contender| {
                let (pool, token, contender, barrier) = (pool.clone(), created.token.clone(), *contender, barrier.clone());
                tokio::spawn(async move {
                    barrier.wait().await;
                    accept_invitation(&pool, &token, contender, None).await
                })
            })
            .collect();
        let mut winners = 0;
        for handle in handles {
            match handle.await? {
                Ok(_) => winners += 1,
                Err(error) => assert_eq!(error, InvitationError::Refused),
            }
        }
        assert_eq!(winners, 1);
        let members: i64 = sqlx::query_scalar("SELECT count(*) FROM team_members WHERE team_id = $1::uuid AND role = 'editor'")
            .bind(team.to_string())
            .fetch_one(&admin_pool().await?)
            .await?;
        assert_eq!(members, 1);
        let tenant_members: i64 = sqlx::query_scalar("SELECT count(*) FROM tenant_members WHERE tenant_id = $1::uuid")
            .bind(tenant_id.to_string())
            .fetch_one(&admin_pool().await?)
            .await?;
        assert_eq!(tenant_members, 2);
        assert_eq!(audit_count(&pool, tenant_id, "invitation.accept").await?, 1);
    }
    Ok(())
}

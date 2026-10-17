//! S4-022: cross-tenant isolation of every tenant-scoped repository module, against the dev
//! PostgreSQL and object store (fixture prefix d6; skipped without FIGCRAFT_TEST_DATABASE_URL_FILE
//! and FIGCRAFT_TEST_S3_DIR).
//!
//! Each run creates two fresh tenants, A and B, each with an owner. Tenant A gets one of every
//! resource the repositories manage (workspace, project, document, member, permission, team, team
//! grant, invitation, share link, service account and credential, asset object with a reference,
//! upload reservation, export and its job, library, external mapping, idempotency key and event
//! log). Then, from inside tenant B's transaction context (and with B's owner, and with A's owner
//! acting in B), every public repository function that takes a [`TenantTransaction`] and an id is
//! called with A's ids: nothing is read, listed, changed, deleted, referenced or presigned. After
//! the attempts, A's resources are read back from A and are unchanged. Object and job keys are
//! checked to carry the tenant.
//!
//! Every attempt runs in its own B transaction that is rolled back, so an error in one cannot hide
//! the next. Every id is random with the d6 prefix, so reruns pass single-threaded or in parallel.
//!
//! Not covered here (no fixture is cheap enough to build in this suite; each has its own suite with
//! a cross-tenant case): `plugin_store` installs (tests/plugin_store.rs), `document_snapshots`
//! stored snapshots (tests/document_snapshots.rs), `libraries::publish_library_version`
//! (tests/libraries.rs). Their read paths are still called here with A's ids.

use std::time::Duration;

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, ProjectId, Role, TenantId, Uuid, WorkspaceId};
use figcraft_cloud_storage::assets::{
    attach_asset, detach_asset, find_asset, list_document_assets, presign_document_asset_download, store_asset_bytes,
};
use figcraft_cloud_storage::document_snapshots::restore_latest_snapshot;
use figcraft_cloud_storage::downloads::{authorize_download, presign_download};
use figcraft_cloud_storage::effective_roles::{
    resolve_document_access, resolve_project_access, resolve_team_role, resolve_tenant_access, resolve_workspace_access,
};
use figcraft_cloud_storage::events::{claim_sequencer, read_events};
use figcraft_cloud_storage::exports::{ExportFormat, export_key, fail_export, get_export, latest_thumbnail, request_export, start_export};
use figcraft_cloud_storage::external_mappings::{create_external_mapping, get_external_mapping};
use figcraft_cloud_storage::idempotency::{ReservationOutcome, reserve_idempotency_key};
use figcraft_cloud_storage::invitations::{create_invitation, list_pending_invitations, revoke_invitation};
use figcraft_cloud_storage::libraries::{
    Change as LibChange, archive_library, create_library, get_library, grant_library_to_team, list_grants, list_versions, remove_library_usage,
    revoke_library_grant, usage_of_library,
};
use figcraft_cloud_storage::memberships::{
    MemberPermission, add_tenant_member, change_tenant_member_role, get_tenant_member, grant_member_permission, list_member_permissions,
    list_tenant_members, reactivate_tenant_member, remove_document_member, remove_document_team_grant, remove_tenant_member, remove_workspace_member,
    remove_workspace_team_grant, revoke_member_permission, set_document_member, set_document_team_grant, set_workspace_member,
    set_workspace_team_grant, suspend_tenant_member,
};
use figcraft_cloud_storage::metadata::{
    PageSize, archive_document, create_document, create_project, create_workspace, get_document, get_project, get_tenant, get_workspace,
    list_projects, list_workspaces, rename_document, restore_document, soft_delete_document,
};
use figcraft_cloud_storage::object_metadata::ObjectKind;
use figcraft_cloud_storage::object_metadata::{get_object, verify_object};
use figcraft_cloud_storage::object_references::{
    Change as RefChange, OwnerKind, add_reference, begin_object_deletion, collectable_objects, list_references, list_retention_holds,
    place_retention_hold, reference_count, release_retention_hold, remove_reference,
};
use figcraft_cloud_storage::objects::{ObjectStore, ObjectStoreSettings};
use figcraft_cloud_storage::plugin_store::{PluginName, find_plugin, get_manifest, get_plugin};
use figcraft_cloud_storage::service_accounts::{
    Scope, authenticate_credential, create_service_account, issue_credential, revoke_credential, rotate_credential, set_service_account_enabled,
};
use figcraft_cloud_storage::share_links::{
    ShareLinkUpdate, ShareRole, create_share_link, dead_share_links, exchange_share_token, list_share_links, revoke_share_link, update_share_link,
};
use figcraft_cloud_storage::snapshots::{reject_snapshot, set_current_snapshot, verify_snapshot};
use figcraft_cloud_storage::teams::{
    GrantTarget, OwnedResource, TeamGrant, TeamRole, WorkspaceGrantRole, add_team_member, archive_team, change_team_member_role, create_team,
    get_team, grant_to_team, list_team_members, list_teams, remove_team_member, rename_team, revoke_team_grant, set_owning_team,
};
use figcraft_cloud_storage::tenants::{NewTenant, create_tenant_with_owner, list_tenants_for_principal};
use figcraft_cloud_storage::uploads::{complete_upload, get_reservation, reserve_upload};
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgPool, PgPoolOptions};

type Error = Box<dyn std::error::Error>;
type Fallible<T> = Result<T, Error>;
type TestResult = Fallible<()>;

const PRODUCER: &str = "figcraft-cloud d6 cross-tenant probe";

/// A random UUIDv4 whose first byte is the d6 fixture prefix.
fn d6() -> Uuid {
    let mut bytes = *Uuid::new_v4().as_bytes();
    bytes[0] = 0xd6;
    Uuid::from_bytes(bytes)
}

fn suffix() -> String {
    d6().simple().to_string()[2..14].to_owned()
}

async fn probe() -> Fallible<Option<(PgPool, ObjectStore)>> {
    let (Ok(database_url_file), Ok(s3_directory)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR")) else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; cross-tenant isolation probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    let pool = PgPoolOptions::new().max_connections(4).connect(database_url.trim_end()).await?;
    let read =
        |name: &str| -> Fallible<String> { Ok(std::fs::read_to_string(std::path::Path::new(&s3_directory).join(name))?.trim_end().to_string()) };
    let settings = ObjectStoreSettings {
        endpoint: read("s3_endpoint")?,
        region: "us-east-1".into(),
        bucket: "figcraft".into(),
        access_key: read("s3_access_key")?,
        secret_key: read("s3_secret_key")?,
        path_style: true,
    };
    Ok(Some((pool, ObjectStore::new(&settings)?)))
}

async fn tx(pool: &PgPool, tenant: TenantId) -> Fallible<TenantTransaction<'_>> {
    begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}").into())
}

/// Run `$body` with `$t` bound to a fresh transaction in `$tenant`, then roll it back (so neither a
/// failed statement nor a wrongly successful write survives).
macro_rules! within {
    ($pool:expr, $tenant:expr, |$t:ident| $body:expr) => {{
        let mut $t = tx($pool, $tenant).await?;
        let result = $body;
        $t.rollback().await?;
        result
    }};
}

async fn principal(pool: &PgPool) -> Fallible<PrincipalId> {
    let id = d6();
    sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'd6 cross-tenant probe')")
        .bind(id.to_string())
        .execute(pool)
        .await?;
    Ok(PrincipalId::try_from(id).map_err(|_| "nil principal")?)
}

/// A fresh active tenant owned by a fresh principal.
async fn tenant(pool: &PgPool) -> Fallible<(TenantId, PrincipalId)> {
    let owner = principal(pool).await?;
    let id = TenantId::try_from(d6()).map_err(|_| "nil tenant")?;
    let slug = format!("d6-{}", suffix());
    let mut transaction = tx(pool, id).await?;
    create_tenant_with_owner(&mut transaction, NewTenant { slug: &slug, name: "d6 cross-tenant", plan_id: "team", home_region: "local" }, owner)
        .await?;
    transaction.commit().await?;
    Ok((id, owner))
}

/// A tiny valid PNG whose bytes are unique to this run (assets are content-addressed).
fn unique_png(label: &str) -> Vec<u8> {
    fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        png.extend_from_slice(&u32::try_from(data.len()).unwrap_or(u32::MAX).to_be_bytes());
        let mut crc = crc32fast::Hasher::new();
        crc.update(kind);
        crc.update(data);
        png.extend_from_slice(kind);
        png.extend_from_slice(data);
        png.extend_from_slice(&crc.finalize().to_be_bytes());
    }
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut png, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 0, 0, 0, 0]);
    chunk(&mut png, b"tEXt", format!("Comment\0figcraft d6 probe {label} {}", Uuid::new_v4()).as_bytes());
    chunk(&mut png, b"IDAT", &[0x78, 0x01, 0x01, 0x02, 0x00, 0xfd, 0xff, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01]);
    chunk(&mut png, b"IEND", &[]);
    png
}

/// Tenant A's resources.
struct World {
    a: TenantId,
    a_owner: PrincipalId,
    a_member: PrincipalId,
    b: TenantId,
    b_owner: PrincipalId,
    workspace: WorkspaceId,
    project: ProjectId,
    document: CloudDocumentId,
    team: Uuid,
    invitation: Uuid,
    link: Uuid,
    link_secret: String,
    account: Uuid,
    credential: Uuid,
    credential_secret: String,
    object: Uuid,
    object_sha: [u8; 32],
    object_key: String,
    reservation: Uuid,
    staging_key: String,
    export: Uuid,
    export_job: Option<Uuid>,
    library: Uuid,
    mapping_external_id: String,
    b_workspace: WorkspaceId,
    b_project: ProjectId,
    b_document: CloudDocumentId,
    b_team: Uuid,
    keys: Vec<String>,
}

async fn build(pool: &PgPool, store: &ObjectStore) -> Fallible<World> {
    let (a, a_owner) = tenant(pool).await?;
    let (b, b_owner) = tenant(pool).await?;
    let a_member = principal(pool).await?;
    let mut t = tx(pool, a).await?;
    add_tenant_member(&mut t, a_member, Role::Editor).await?;
    grant_member_permission(&mut t, a_member, MemberPermission::Export, a_owner).await?;
    let workspace = WorkspaceId::try_from(d6()).map_err(|_| "nil")?;
    create_workspace(&mut t, workspace, "d6 workspace A", None).await?;
    let project = ProjectId::try_from(d6()).map_err(|_| "nil")?;
    create_project(&mut t, project, workspace, "d6 project A", None).await?;
    let document = CloudDocumentId::random();
    create_document(&mut t, document, workspace, project, "d6 document A").await?;
    set_workspace_member(&mut t, workspace, a_member, Role::Editor).await?;
    set_document_member(&mut t, document, a_member, Role::Editor).await?;
    let team = d6();
    create_team(&mut t, team, &format!("d6-{}", suffix()), "d6 team A", a_owner).await?;
    add_team_member(&mut t, team, a_member, TeamRole::Editor, a_owner).await?;
    grant_to_team(&mut t, team, TeamGrant::Workspace(workspace, WorkspaceGrantRole::Editor), a_owner).await?;
    set_document_team_grant(&mut t, document, team, Role::Viewer).await?;
    let invitation = create_invitation(&mut t, team, &format!("d6-{}@example.test", suffix()), TeamRole::Viewer, a_owner, None).await?.id;
    let link = create_share_link(&mut t, document, ShareRole::Editor, Some("d6 A"), a_owner, None).await?;
    let account = create_service_account(&mut t, "d6 A robot", &[Scope::DocumentsRead], a_owner).await?;
    let credential = issue_credential(&mut t, account.id, 3600, a_owner).await?;
    let stored = store_asset_bytes(&mut t, store, unique_png("A"), "image/png", PRODUCER).await?.object;
    attach_asset(&mut t, document, stored.id).await?;
    let png = unique_png("A upload");
    let sha: [u8; 32] = Sha256::digest(&png).into();
    let reserved =
        reserve_upload(&mut t, store, a_owner, ObjectKind::Asset, sha, i64::try_from(png.len())?, "image/png", Duration::from_secs(600)).await?;
    let export = request_export(&mut t, document, a_owner, ExportFormat::Png, &serde_json::json!({}), 0).await?;
    let library = create_library(&mut t, Some(team), Some(document), "d6 library A").await?.id;
    let mapping_external_id = format!("d6-ext-{}", suffix());
    create_external_mapping(&mut t, "d6-system", "document", &mapping_external_id, document.as_uuid()).await?;
    t.commit().await?;

    let mut t = tx(pool, b).await?;
    let b_workspace = WorkspaceId::try_from(d6()).map_err(|_| "nil")?;
    create_workspace(&mut t, b_workspace, "d6 workspace B", None).await?;
    let b_project = ProjectId::try_from(d6()).map_err(|_| "nil")?;
    create_project(&mut t, b_project, b_workspace, "d6 project B", None).await?;
    let b_document = CloudDocumentId::random();
    create_document(&mut t, b_document, b_workspace, b_project, "d6 document B").await?;
    let b_team = d6();
    create_team(&mut t, b_team, &format!("d6-{}", suffix()), "d6 team B", b_owner).await?;
    t.commit().await?;

    Ok(World {
        a,
        a_owner,
        a_member,
        b,
        b_owner,
        workspace,
        project,
        document,
        team,
        invitation,
        link: link.id,
        link_secret: link.secret,
        account: account.id,
        credential: credential.id,
        credential_secret: credential.secret,
        object: stored.id,
        object_sha: stored.sha256,
        object_key: stored.object_key.clone(),
        reservation: reserved.reservation.id,
        staging_key: reserved.reservation.staging_key.clone(),
        export: export.id,
        export_job: export.job_id,
        library,
        mapping_external_id,
        b_workspace,
        b_project,
        b_document,
        b_team,
        keys: vec![stored.object_key],
    })
}

/// Ok(None), Ok(false), Ok(empty) or Err: anything that is not a visible result counts as
/// refused. A visible result (Some/true/non-empty) fails the test with `what`.
fn refused<T: std::fmt::Debug, E: std::fmt::Debug>(what: &str, result: &Result<T, E>, visible: impl Fn(&T) -> bool) -> TestResult {
    if let Ok(value) = result
        && visible(value)
    {
        return Err(format!("tenant B {what}: saw tenant A data: {value:?}").into());
    }
    Ok(())
}

#[tokio::test]
async fn tenant_b_cannot_read_list_change_reference_or_presign_tenant_a_resources() -> TestResult {
    let Some((pool, store)) = probe().await? else {
        return Ok(());
    };
    let w = build(&pool, &store).await?;
    let pool = &pool;
    let page = PageSize::new(100)?;
    let (b, b_owner, a_owner) = (w.b, w.b_owner, w.a_owner);

    // ---- metadata ----
    assert_eq!(within!(pool, b, |t| get_tenant(&mut t).await?).map(|tenant| tenant.id), Some(b));
    assert!(within!(pool, b, |t| get_workspace(&mut t, w.workspace).await?).is_none());
    let listed = within!(pool, b, |t| list_workspaces(&mut t, None, page).await?);
    assert!(listed.items.iter().all(|item| item.id != w.workspace) && listed.items.iter().any(|item| item.id == w.b_workspace));
    assert!(within!(pool, b, |t| get_project(&mut t, w.workspace, w.project).await?).is_none());
    refused("list_projects", &within!(pool, b, |t| list_projects(&mut t, w.workspace, None, page).await), |p| !p.items.is_empty())?;
    let fresh_project = ProjectId::try_from(d6()).map_err(|_| "nil")?;
    assert!(within!(pool, b, |t| create_project(&mut t, fresh_project, w.workspace, "d6 B into A", None).await).is_err());
    let fresh_document = CloudDocumentId::random();
    assert!(within!(pool, b, |t| create_document(&mut t, fresh_document, w.workspace, w.project, "d6 B into A").await).is_err());
    assert!(within!(pool, b, |t| create_document(&mut t, fresh_document, w.b_workspace, w.project, "d6 mixed").await).is_err());
    assert!(within!(pool, b, |t| create_document(&mut t, fresh_document, w.workspace, w.b_project, "d6 mixed").await).is_err());
    assert!(within!(pool, b, |t| get_document(&mut t, w.document).await?).is_none());
    refused("rename_document", &within!(pool, b, |t| rename_document(&mut t, w.document, "d6 stolen").await), Option::is_some)?;
    refused("archive_document", &within!(pool, b, |t| archive_document(&mut t, w.document).await), Option::is_some)?;
    refused("restore_document", &within!(pool, b, |t| restore_document(&mut t, w.document).await), Option::is_some)?;
    refused("soft_delete_document", &within!(pool, b, |t| soft_delete_document(&mut t, w.document).await), Option::is_some)?;
    let a_tenants = list_tenants_for_principal(pool, a_owner, None, page).await?;
    assert!(
        a_tenants.items.iter().all(|membership| membership.tenant.id != b) && a_tenants.items.iter().any(|membership| membership.tenant.id == w.a)
    );

    // ---- memberships and permissions ----
    for principal in [a_owner, w.a_member] {
        assert!(within!(pool, b, |t| get_tenant_member(&mut t, principal).await?).is_none());
        refused("change_tenant_member_role", &within!(pool, b, |t| change_tenant_member_role(&mut t, principal, Role::Viewer).await), |()| true)?;
        refused("suspend_tenant_member", &within!(pool, b, |t| suspend_tenant_member(&mut t, principal).await), |()| true)?;
        refused("reactivate_tenant_member", &within!(pool, b, |t| reactivate_tenant_member(&mut t, principal).await), |()| true)?;
        refused("remove_tenant_member", &within!(pool, b, |t| remove_tenant_member(&mut t, principal).await), |()| true)?;
        refused(
            "grant_member_permission",
            &within!(pool, b, |t| grant_member_permission(&mut t, principal, MemberPermission::Billing, b_owner).await),
            |_| true,
        )?;
        refused(
            "revoke_member_permission",
            &within!(pool, b, |t| revoke_member_permission(&mut t, principal, MemberPermission::Export).await),
            |r| *r,
        )?;
        refused("list_member_permissions", &within!(pool, b, |t| list_member_permissions(&mut t, principal).await), |r| !r.is_empty())?;
    }
    let members = within!(pool, b, |t| list_tenant_members(&mut t, None).await?);
    assert!(members.iter().all(|member| member.principal_id != a_owner && member.principal_id != w.a_member));
    refused("set_workspace_member", &within!(pool, b, |t| set_workspace_member(&mut t, w.workspace, b_owner, Role::Editor).await), |()| true)?;
    refused("remove_workspace_member", &within!(pool, b, |t| remove_workspace_member(&mut t, w.workspace, w.a_member).await), |r| *r)?;
    refused("set_document_member", &within!(pool, b, |t| set_document_member(&mut t, w.document, b_owner, Role::Editor).await), |()| true)?;
    refused("remove_document_member", &within!(pool, b, |t| remove_document_member(&mut t, w.document, w.a_member).await), |r| *r)?;
    refused(
        "set_workspace_team_grant A ws",
        &within!(pool, b, |t| set_workspace_team_grant(&mut t, w.workspace, w.b_team, Role::Editor).await),
        |()| true,
    )?;
    refused(
        "set_workspace_team_grant A team",
        &within!(pool, b, |t| set_workspace_team_grant(&mut t, w.b_workspace, w.team, Role::Editor).await),
        |()| true,
    )?;
    refused("remove_workspace_team_grant", &within!(pool, b, |t| remove_workspace_team_grant(&mut t, w.workspace, w.team).await), |r| *r)?;
    refused(
        "set_document_team_grant A doc",
        &within!(pool, b, |t| set_document_team_grant(&mut t, w.document, w.b_team, Role::Viewer).await),
        |()| true,
    )?;
    refused(
        "set_document_team_grant A team",
        &within!(pool, b, |t| set_document_team_grant(&mut t, w.b_document, w.team, Role::Viewer).await),
        |()| true,
    )?;
    refused("remove_document_team_grant", &within!(pool, b, |t| remove_document_team_grant(&mut t, w.document, w.team).await), |r| *r)?;

    // ---- effective roles: nobody resolves anything of A's from B, not even A's own owner ----
    for principal in [a_owner, w.a_member, b_owner] {
        refused("resolve_workspace_access", &within!(pool, b, |t| resolve_workspace_access(&mut t, principal, w.workspace).await), Option::is_some)?;
        refused("resolve_project_access", &within!(pool, b, |t| resolve_project_access(&mut t, principal, w.project).await), Option::is_some)?;
        refused("resolve_document_access", &within!(pool, b, |t| resolve_document_access(&mut t, principal, w.document).await), Option::is_some)?;
        refused("resolve_team_role", &within!(pool, b, |t| resolve_team_role(&mut t, principal, w.team).await), Option::is_some)?;
    }
    // A principal with no membership resolves no role and no permission (the record of the
    // principal's and tenant's state alone grants nothing).
    let grants_anything = |access: &Option<figcraft_cloud_storage::effective_roles::TenantAccess>| {
        access.as_ref().is_some_and(|access| access.tenant_role.is_some() || !access.permissions.is_empty())
    };
    refused("resolve_tenant_access A owner in B", &within!(pool, b, |t| resolve_tenant_access(&mut t, a_owner).await), grants_anything)?;
    // ... and B's owner resolves nothing in A.
    refused("resolve_tenant_access B owner in A", &within!(pool, w.a, |t| resolve_tenant_access(&mut t, b_owner).await), grants_anything)?;
    // In A's own context the document exists (Some), but an outsider gets no role and no source,
    // which the API's disclosure policy turns into 404.
    refused("resolve_document_access B owner in A", &within!(pool, w.a, |t| resolve_document_access(&mut t, b_owner, w.document).await), |access| {
        access.as_ref().is_some_and(|access| access.role.is_some() || !access.sources.is_empty() || access.tenant.tenant_role.is_some())
    })?;

    // ---- teams ----
    assert!(within!(pool, b, |t| get_team(&mut t, w.team).await?).is_none());
    let teams = within!(pool, b, |t| list_teams(&mut t, None, page).await?);
    assert!(teams.items.iter().all(|team| team.id != w.team));
    refused("rename_team", &within!(pool, b, |t| rename_team(&mut t, w.team, "d6 stolen", b_owner).await), |_| true)?;
    refused("archive_team", &within!(pool, b, |t| archive_team(&mut t, w.team, b_owner).await), |_| true)?;
    refused("add_team_member", &within!(pool, b, |t| add_team_member(&mut t, w.team, b_owner, TeamRole::TeamManager, b_owner).await), |()| true)?;
    refused("remove_team_member", &within!(pool, b, |t| remove_team_member(&mut t, w.team, w.a_member, b_owner).await), |r| *r)?;
    refused(
        "change_team_member_role",
        &within!(pool, b, |t| change_team_member_role(&mut t, w.team, w.a_member, TeamRole::TeamManager, b_owner).await),
        |()| true,
    )?;
    refused("list_team_members", &within!(pool, b, |t| list_team_members(&mut t, w.team, None, page).await), |p| !p.items.is_empty())?;
    refused(
        "set_owning_team A ws",
        &within!(pool, b, |t| set_owning_team(&mut t, OwnedResource::Workspace(w.workspace), Some(w.b_team), b_owner).await),
        |()| true,
    )?;
    refused(
        "set_owning_team A team",
        &within!(pool, b, |t| set_owning_team(&mut t, OwnedResource::Workspace(w.b_workspace), Some(w.team), b_owner).await),
        |()| true,
    )?;
    refused(
        "set_owning_team A library",
        &within!(pool, b, |t| set_owning_team(&mut t, OwnedResource::Library(w.library), Some(w.b_team), b_owner).await),
        |()| true,
    )?;
    refused(
        "grant_to_team A ws",
        &within!(pool, b, |t| grant_to_team(&mut t, w.b_team, TeamGrant::Workspace(w.workspace, WorkspaceGrantRole::Editor), b_owner).await),
        |()| true,
    )?;
    refused(
        "grant_to_team A team",
        &within!(pool, b, |t| grant_to_team(&mut t, w.team, TeamGrant::Workspace(w.b_workspace, WorkspaceGrantRole::Editor), b_owner).await),
        |()| true,
    )?;
    refused(
        "revoke_team_grant",
        &within!(pool, b, |t| revoke_team_grant(&mut t, w.team, GrantTarget::Workspace(w.workspace), b_owner).await),
        |r| *r,
    )?;

    // ---- invitations ----
    refused(
        "create_invitation",
        &within!(pool, b, |t| create_invitation(&mut t, w.team, "d6-b@example.test", TeamRole::Viewer, b_owner, None).await),
        |_| true,
    )?;
    refused("revoke_invitation", &within!(pool, b, |t| revoke_invitation(&mut t, w.invitation, b_owner).await), |r| *r)?;
    refused("list_pending_invitations team", &within!(pool, b, |t| list_pending_invitations(&mut t, Some(w.team), None, page).await), |p| {
        !p.items.is_empty()
    })?;
    let pending = within!(pool, b, |t| list_pending_invitations(&mut t, None, None, page).await?);
    assert!(pending.items.iter().all(|invitation| invitation.id != w.invitation));

    // ---- share links ----
    refused("create_share_link", &within!(pool, b, |t| create_share_link(&mut t, w.document, ShareRole::Editor, None, b_owner, None).await), |_| {
        true
    })?;
    refused("list_share_links", &within!(pool, b, |t| list_share_links(&mut t, w.document).await), |r| !r.is_empty())?;
    let update = ShareLinkUpdate { label: Some(Some("d6 stolen")), expires_in_secs: None };
    refused("update_share_link", &within!(pool, b, |t| update_share_link(&mut t, w.link, &update).await), Option::is_some)?;
    refused("revoke_share_link", &within!(pool, b, |t| revoke_share_link(&mut t, w.link).await), |r| *r)?;
    // A link presented with the wrong tenant is dead to the relay's revocation check.
    assert_eq!(dead_share_links(pool, &[(b, w.link)]).await?, vec![(b, w.link)]);
    assert!(dead_share_links(pool, &[(w.a, w.link)]).await?.is_empty());

    // ---- service accounts ----
    refused("issue_credential", &within!(pool, b, |t| issue_credential(&mut t, w.account, 3600, b_owner).await), |_| true)?;
    refused("rotate_credential", &within!(pool, b, |t| rotate_credential(&mut t, w.account, w.credential, 3600, 60, b_owner).await), |_| true)?;
    refused("revoke_credential", &within!(pool, b, |t| revoke_credential(&mut t, w.account, w.credential, b_owner).await), |r| *r)?;
    refused("set_service_account_enabled", &within!(pool, b, |t| set_service_account_enabled(&mut t, w.account, false, b_owner).await), |r| *r)?;

    // ---- objects, assets, uploads, downloads ----
    assert!(within!(pool, b, |t| get_object(&mut t, w.object).await?).is_none());
    refused("verify_object", &within!(pool, b, |t| verify_object(&mut t, &store, w.object).await), Option::is_some)?;
    refused("find_asset", &within!(pool, b, |t| find_asset(&mut t, &w.object_sha).await), Option::is_some)?;
    assert!(within!(pool, b, |t| attach_asset(&mut t, w.b_document, w.object).await).is_err());
    assert!(within!(pool, b, |t| attach_asset(&mut t, w.document, w.object).await).is_err());
    assert!(within!(pool, b, |t| detach_asset(&mut t, w.document, w.object).await).is_err());
    refused("list_document_assets", &within!(pool, b, |t| list_document_assets(&mut t, w.document, None, 50).await), |p| !p.assets.is_empty())?;
    for (document, principal) in [(w.document, b_owner), (w.b_document, b_owner), (w.document, a_owner)] {
        refused(
            "presign_document_asset_download",
            &within!(pool, b, |t| presign_document_asset_download(&mut t, &store, principal, document, w.object, None).await),
            |_| true,
        )?;
    }
    for principal in [b_owner, a_owner] {
        assert!(within!(pool, b, |t| authorize_download(&mut t, principal, w.object).await).is_err());
    }
    // B's owner can't download from A even inside A's context.
    assert!(within!(pool, w.a, |t| authorize_download(&mut t, b_owner, w.object).await).is_err());
    // A grant made in A can't be redeemed in B's transaction.
    let grant = within!(pool, w.a, |t| authorize_download(&mut t, a_owner, w.object).await?);
    refused("presign_download with A's grant in B", &within!(pool, b, |t| presign_download(&mut t, &store, grant, w.object, None).await), |_| true)?;
    refused("get_reservation", &within!(pool, b, |t| get_reservation(&mut t, w.reservation).await), Option::is_some)?;
    refused("complete_upload", &within!(pool, b, |t| complete_upload(&mut t, &store, w.reservation, b_owner, PRODUCER).await), |_| true)?;

    // ---- object references and retention ----
    refused("add_reference", &within!(pool, b, |t| add_reference(&mut t, w.object, OwnerKind::Document, w.b_document.as_uuid()).await), |_| true)?;
    refused("remove_reference", &within!(pool, b, |t| remove_reference(&mut t, w.object, OwnerKind::Asset, w.document.as_uuid()).await), |c| {
        *c == RefChange::Applied
    })?;
    let hold = d6();
    refused(
        "place_retention_hold",
        &within!(pool, b, |t| place_retention_hold(&mut t, w.object, hold, b_owner.as_uuid(), "d6 legal hold").await),
        |_| true,
    )?;
    refused("release_retention_hold", &within!(pool, b, |t| release_retention_hold(&mut t, w.object, hold).await), |c| *c == RefChange::Applied)?;
    refused("list_retention_holds", &within!(pool, b, |t| list_retention_holds(&mut t, w.object, 10).await), |r| !r.is_empty())?;
    refused("reference_count", &within!(pool, b, |t| reference_count(&mut t, w.object).await), |count| !count.is_unreferenced())?;
    refused("list_references", &within!(pool, b, |t| list_references(&mut t, w.object, 10).await), |r| !r.is_empty())?;
    refused("begin_object_deletion", &within!(pool, b, |t| begin_object_deletion(&mut t, w.object).await), |_| true)?;
    let collectable = within!(pool, b, |t| collectable_objects(&mut t, Duration::ZERO, 1000).await?);
    assert!(collectable.iter().all(|object| object.id != w.object), "{collectable:?}");

    // ---- exports, jobs and thumbnails ----
    refused(
        "request_export",
        &within!(pool, b, |t| request_export(&mut t, w.document, b_owner, ExportFormat::Png, &serde_json::json!({}), 0).await),
        |_| true,
    )?;
    refused(
        "request_export (A owner)",
        &within!(pool, b, |t| request_export(&mut t, w.document, a_owner, ExportFormat::Png, &serde_json::json!({}), 0).await),
        |_| true,
    )?;
    refused("get_export", &within!(pool, b, |t| get_export(&mut t, w.export).await), Option::is_some)?;
    refused("start_export", &within!(pool, b, |t| start_export(&mut t, w.export).await), |_| true)?;
    refused("fail_export", &within!(pool, b, |t| fail_export(&mut t, w.export, "d6_probe").await), |_| true)?;
    refused("latest_thumbnail", &within!(pool, b, |t| latest_thumbnail(&mut t, w.document).await), Option::is_some)?;

    // ---- events and snapshots ----
    refused("claim_sequencer", &within!(pool, b, |t| claim_sequencer(&mut t, w.document).await), |_| true)?;
    refused("read_events", &within!(pool, b, |t| read_events(&mut t, w.document, 0, 16, 1 << 20).await), |p| !p.events.is_empty())?;
    let snapshot = d6();
    refused("verify_snapshot", &within!(pool, b, |t| verify_snapshot(&mut t, snapshot, w.document).await), Option::is_some)?;
    refused("reject_snapshot", &within!(pool, b, |t| reject_snapshot(&mut t, snapshot, w.document).await), Option::is_some)?;
    // set_current_snapshot reports Ok for a document outside the tenant (it is a silent no-op), so
    // commit the attempt and check A's document didn't change.
    let mut t = tx(pool, b).await?;
    let _ = set_current_snapshot(&mut t, w.document, snapshot).await;
    t.commit().await?;
    let current: Option<String> =
        within!(pool, w.a, |t| sqlx::query_scalar("SELECT current_snapshot_id::text FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid")
            .bind(w.a.to_string())
            .bind(w.document.to_string())
            .fetch_one(t.connection())
            .await?);
    assert_eq!(current, None);
    refused("restore_latest_snapshot", &within!(pool, b, |t| restore_latest_snapshot(&mut t, &store, w.document).await), |r| r.restored.is_some())?;

    // ---- libraries and plugins ----
    refused("get_library", &within!(pool, b, |t| get_library(&mut t, w.library).await), Option::is_some)?;
    refused("archive_library", &within!(pool, b, |t| archive_library(&mut t, w.library).await), |_| true)?;
    refused(
        "grant_library_to_team",
        &within!(pool, b, |t| grant_library_to_team(&mut t, w.library, w.b_team, figcraft_cloud_storage::libraries::LibraryRole::Viewer).await),
        |_| true,
    )?;
    refused("revoke_library_grant", &within!(pool, b, |t| revoke_library_grant(&mut t, w.library, w.team).await), |c| *c == LibChange::Applied)?;
    refused("list_grants", &within!(pool, b, |t| list_grants(&mut t, w.library, 10).await), |r| !r.is_empty())?;
    refused("list_versions", &within!(pool, b, |t| list_versions(&mut t, w.library, None, 10).await), |r| !r.is_empty())?;
    refused("usage_of_library", &within!(pool, b, |t| usage_of_library(&mut t, w.document, w.library).await), Option::is_some)?;
    refused("remove_library_usage", &within!(pool, b, |t| remove_library_usage(&mut t, w.document, w.library).await), |c| *c == LibChange::Applied)?;
    refused("get_plugin", &within!(pool, b, |t| get_plugin(&mut t, w.library).await), Option::is_some)?;
    refused("find_plugin", &within!(pool, b, |t| find_plugin(&mut t, PluginName { publisher: "d6", name: "absent" }).await), Option::is_some)?;
    refused("get_manifest", &within!(pool, b, |t| get_manifest(&mut t, w.library, "1.0.0").await), Option::is_some)?;

    // ---- external mappings and idempotency are namespaced by tenant ----
    assert!(within!(pool, b, |t| get_external_mapping(&mut t, "d6-system", "document", &w.mapping_external_id).await?).is_none());
    let b_mapping =
        within!(pool, b, |t| create_external_mapping(&mut t, "d6-system", "document", &w.mapping_external_id, w.b_document.as_uuid()).await?);
    assert_eq!((b_mapping.tenant_id, b_mapping.internal_id), (b, w.b_document.as_uuid()));
    let key_hash = Sha256::digest(format!("d6 idem {}", suffix())).to_vec();
    let request_hash = Sha256::digest(b"d6 request").to_vec();
    let mut t = tx(pool, w.a).await?;
    assert_eq!(reserve_idempotency_key(&mut t, a_owner, "d6.route", &key_hash, &request_hash, 3600).await?, ReservationOutcome::Reserved);
    t.commit().await?;
    // A's principal can't hold a key in B at all (keys reference a membership of their tenant) ...
    refused(
        "reserve_idempotency_key A owner",
        &within!(pool, b, |t| reserve_idempotency_key(&mut t, a_owner, "d6.route", &key_hash, &request_hash, 3600).await),
        |_| true,
    )?;
    // ... and the same route and key in B is a fresh reservation, never A's in-flight entry.
    assert_eq!(
        within!(pool, b, |t| reserve_idempotency_key(&mut t, b_owner, "d6.route", &key_hash, &request_hash, 3600).await?),
        ReservationOutcome::Reserved
    );

    // ---- keys carry the tenant ----
    let prefix = format!("tenants/{}/", w.a);
    assert!(w.object_key.starts_with(&prefix), "{}", w.object_key);
    assert!(w.staging_key.starts_with(&prefix), "{}", w.staging_key);
    assert!(export_key(w.a, w.export, "d6.png").is_some_and(|key| key.starts_with(&prefix)));
    for key in &w.keys {
        assert!(key.starts_with(&prefix), "{key}");
    }
    if let Some(job) = w.export_job {
        let job_tenant: Option<String> = within!(pool, w.a, |t| sqlx::query_scalar("SELECT tenant_id::text FROM jobs WHERE id = $1::uuid")
            .bind(job.to_string())
            .fetch_optional(t.connection())
            .await?);
        assert_eq!(job_tenant, Some(w.a.to_string()));
        // B's context can't see A's job row.
        let seen: i64 = within!(pool, b, |t| sqlx::query_scalar("SELECT count(*) FROM jobs WHERE id = $1::uuid AND tenant_id = $2::uuid")
            .bind(job.to_string())
            .bind(b.to_string())
            .fetch_one(t.connection())
            .await?);
        assert_eq!(seen, 0);
    }

    // ---- presented secrets authenticate to their own tenant only ----
    assert_eq!(exchange_share_token(pool, &w.link_secret).await?.tenant_id, w.a);
    assert_eq!(authenticate_credential(pool, &w.credential_secret).await?.tenant_id, w.a);

    // ---- A is untouched ----
    let mut t = tx(pool, w.a).await?;
    let document = get_document(&mut t, w.document).await?.ok_or("A's document is gone")?;
    assert_eq!(document.name, "d6 document A");
    assert_eq!(document.state, figcraft_cloud_domain::ResourceState::Active);
    assert!(get_workspace(&mut t, w.workspace).await?.is_some());
    assert!(get_project(&mut t, w.workspace, w.project).await?.is_some());
    assert_eq!(get_tenant_member(&mut t, w.a_member).await?.map(|member| member.role), Some(Role::Editor));
    assert_eq!(list_member_permissions(&mut t, w.a_member).await?, vec![MemberPermission::Export]);
    assert_eq!(get_team(&mut t, w.team).await?.map(|team| team.name), Some("d6 team A".to_string()));
    assert_eq!(list_team_members(&mut t, w.team, None, page).await?.items.len(), 2);
    let invitations = list_pending_invitations(&mut t, Some(w.team), None, page).await?;
    assert_eq!(invitations.items.iter().map(|invitation| invitation.id).collect::<Vec<_>>(), vec![w.invitation]);
    let links = list_share_links(&mut t, w.document).await?;
    assert_eq!(links.len(), 1);
    assert_eq!((links[0].label.as_deref(), links[0].revoked_at), (Some("d6 A"), None));
    assert!(get_object(&mut t, w.object).await?.is_some());
    assert_eq!(list_document_assets(&mut t, w.document, None, 50).await?.assets.len(), 1);
    assert!(!reference_count(&mut t, w.object).await?.is_unreferenced());
    assert!(get_reservation(&mut t, w.reservation).await?.is_some());
    assert!(get_export(&mut t, w.export).await?.is_some());
    assert!(get_library(&mut t, w.library).await?.is_some());
    assert!(get_external_mapping(&mut t, "d6-system", "document", &w.mapping_external_id).await?.is_some());
    let access = resolve_document_access(&mut t, w.a_member, w.document).await?.ok_or("A member lost access")?;
    assert_eq!(access.role, Some(Role::Editor));
    t.rollback().await?;
    Ok(())
}

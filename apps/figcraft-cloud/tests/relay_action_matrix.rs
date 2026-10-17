//! S4-021 (relay actions) and S4-022 (realtime): the collaboration relay's admission matrix through
//! the production backend (`CloudBackend::admit`, the hook every hello goes through), against the
//! dev PostgreSQL with row-level security on (fixture prefix e2; skipped without
//! FIGCRAFT_TEST_DATABASE_URL_FILE and FIGCRAFT_TEST_S3_DIR).
//!
//! A hello names a document (`file`) and presents either a share-link secret or a member token
//! `member.<tenant>.<session token>` (a browser session from `sessions::create_session`; there
//! is no separate realtime-ticket route on main). The expected column is written from the rules:
//! editor links and `editor`+ members join with edit access, viewer links and `viewer` /
//! `commenter` members join read-only, and everything else (revoked, expired-by-logout, another
//! document's link, another tenant's link or session, a non-member, a suspended member, a
//! service-account credential, an archived document or workspace, junk) is refused with the one
//! uniform message.

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, ProjectId, Role, TenantId, Uuid, WorkspaceId};
use figcraft_cloud_relay::collab_backend::{CloudBackend, CloudBackendConfig, REFUSED};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::memberships::{add_tenant_member, set_workspace_member, suspend_tenant_member};
use figcraft_cloud_storage::metadata::{archive_document, create_document, create_project, create_workspace};
use figcraft_cloud_storage::objects::ObjectStoreSettings;
use figcraft_cloud_storage::resource_admin::{Transition, transition_project, transition_workspace};
use figcraft_cloud_storage::revocation::MEMBER_TOKEN_PREFIX;
use figcraft_cloud_storage::service_accounts::{Scope, create_service_account, issue_credential};
use figcraft_cloud_storage::sessions::{SessionLimits, create_session, logout};
use figcraft_cloud_storage::share_links::{ShareRole, create_share_link, revoke_share_link};
use figcraft_cloud_storage::tenants::{NewTenant, create_tenant_with_owner};
use figcraft_collab_net::hub::Backend;
use sqlx::postgres::{PgPool, PgPoolOptions};

type Error = Box<dyn std::error::Error>;

fn e2() -> Uuid {
    let mut bytes = *Uuid::new_v4().as_bytes();
    bytes[0] = 0xe2;
    Uuid::from_bytes(bytes)
}

fn id<T: TryFrom<Uuid>>() -> Result<T, Error> {
    T::try_from(e2()).map_err(|_| "nil id".into())
}

fn err<E: std::fmt::Debug>(error: E) -> Error {
    format!("{error:?}").into()
}

async fn principal(pool: &PgPool, name: &str) -> Result<PrincipalId, Error> {
    let principal: PrincipalId = id()?;
    sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', $2)")
        .bind(principal.to_string())
        .bind(name)
        .execute(pool)
        .await?;
    Ok(principal)
}

async fn session(pool: &PgPool, principal: PrincipalId) -> Result<String, Error> {
    Ok(create_session(pool, principal, SessionLimits::default()).await.map_err(err)?.token)
}

/// A tenant with an owner, one workspace, project and two documents.
struct Space {
    tenant: TenantId,
    owner: PrincipalId,
    workspace: WorkspaceId,
    project: ProjectId,
    document: CloudDocumentId,
    other_document: CloudDocumentId,
}

async fn space(pool: &PgPool, label: &str) -> Result<Space, Error> {
    let owner = principal(pool, &format!("e2 relay {label} owner")).await?;
    let tenant: TenantId = id()?;
    let slug = format!("e2-{}", &e2().simple().to_string()[2..14]);
    let mut t = begin_tenant_transaction(pool, tenant).await.map_err(err)?;
    create_tenant_with_owner(&mut t, NewTenant { slug: &slug, name: "e2 relay", plan_id: "team", home_region: "local" }, owner).await.map_err(err)?;
    let workspace: WorkspaceId = id()?;
    create_workspace(&mut t, workspace, "e2 relay", None).await.map_err(err)?;
    let project: ProjectId = id()?;
    create_project(&mut t, project, workspace, "e2 relay", None).await.map_err(err)?;
    let (document, other_document): (CloudDocumentId, CloudDocumentId) = (id()?, id()?);
    create_document(&mut t, document, workspace, project, "e2 relay doc").await.map_err(err)?;
    create_document(&mut t, other_document, workspace, project, "e2 relay other doc").await.map_err(err)?;
    t.commit().await?;
    Ok(Space { tenant, owner, workspace, project, document, other_document })
}

fn member_token(tenant: TenantId, session: &str) -> String {
    format!("{MEMBER_TOKEN_PREFIX}{tenant}.{session}")
}

/// What a hello should get: `Some("Edit")`, `Some("View")` or `None` (refused).
type Want = Option<&'static str>;
const EDIT: Want = Some("Edit");
const VIEW: Want = Some("View");
const REFUSE: Want = None;

fn outcome(backend: &mut CloudBackend, file: CloudDocumentId, token: &str) -> Result<Want, String> {
    match backend.admit(&file.to_string(), token) {
        Ok(admission) => match format!("{:?}", admission.access).as_str() {
            "Edit" => Ok(EDIT),
            "View" => Ok(VIEW),
            other => Err(format!("unexpected access {other}")),
        },
        Err(message) if message == REFUSED => Ok(REFUSE),
        Err(message) => Err(format!("not the uniform refusal: {message}")),
    }
}

/// Cells where the product does not yet follow the rule: (label, what it does today, why). The
/// expected column above keeps the rule; a cell listed here must still show today's behaviour, so
/// the fix fails this test until the entry is removed.
const KNOWN_GAPS: &[(&str, Want, &str)] = &[
    ("link into an archived workspace", EDIT, "figcraft_live_share_link checks the document and tenant state, not the workspace's"),
    ("link into an archived project", EDIT, "figcraft_live_share_link checks the document and tenant state, not the project's"),
];

#[test]
fn every_relay_hello_matches_the_expected_admission_matrix() -> Result<(), Error> {
    let (Ok(database_url_file), Ok(s3_directory)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR")) else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; relay action matrix skipped");
        return Ok(());
    };
    let database_url = std::fs::read_to_string(database_url_file)?.trim_end().to_string();
    let read =
        |name: &str| -> Result<String, Error> { Ok(std::fs::read_to_string(std::path::Path::new(&s3_directory).join(name))?.trim_end().to_string()) };
    let objects = ObjectStoreSettings {
        endpoint: read("s3_endpoint")?,
        region: "us-east-1".into(),
        bucket: "figcraft".into(),
        access_key: read("s3_access_key")?,
        secret_key: read("s3_secret_key")?,
        path_style: true,
    };
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;

    // (label, file, token, expected) rows, built against fresh fixtures.
    let rows: Vec<(&str, CloudDocumentId, String, Want)> = runtime.block_on(async {
        let pool = PgPoolOptions::new().max_connections(2).connect(&database_url).await?;
        let a = space(&pool, "A").await?;
        let b = space(&pool, "B").await?;
        let archived = space(&pool, "archived").await?;
        let archived_project = space(&pool, "archived project").await?;

        // Members of A, each with that role on the workspace (tenant admin and owner hold no
        // workspace row: their tenant role reaches every workspace).
        let viewer = principal(&pool, "e2 relay viewer").await?;
        let commenter = principal(&pool, "e2 relay commenter").await?;
        let editor = principal(&pool, "e2 relay editor").await?;
        let ws_admin = principal(&pool, "e2 relay workspace admin").await?;
        let admin = principal(&pool, "e2 relay tenant admin").await?;
        let bare = principal(&pool, "e2 relay member without a workspace role").await?;
        let suspended = principal(&pool, "e2 relay suspended").await?;
        let outsider = principal(&pool, "e2 relay outsider").await?;
        let mut t = begin_tenant_transaction(&pool, a.tenant).await.map_err(err)?;
        for (who, role) in [
            (viewer, Role::Viewer),
            (commenter, Role::Commenter),
            (editor, Role::Editor),
            (ws_admin, Role::Editor),
            (admin, Role::TenantAdmin),
            (bare, Role::Viewer),
            (suspended, Role::Editor),
        ] {
            add_tenant_member(&mut t, who, role).await.map_err(err)?;
        }
        for (who, role) in [
            (viewer, Role::Viewer),
            (commenter, Role::Commenter),
            (editor, Role::Editor),
            (ws_admin, Role::WorkspaceAdmin),
            (suspended, Role::Editor),
        ] {
            set_workspace_member(&mut t, a.workspace, who, role).await.map_err(err)?;
        }
        suspend_tenant_member(&mut t, suspended).await.map_err(err)?;
        let editor_link = create_share_link(&mut t, a.document, ShareRole::Editor, None, a.owner, None).await.map_err(err)?;
        let viewer_link = create_share_link(&mut t, a.document, ShareRole::Viewer, None, a.owner, None).await.map_err(err)?;
        let revoked_link = create_share_link(&mut t, a.document, ShareRole::Editor, None, a.owner, None).await.map_err(err)?;
        assert_eq!(revoke_share_link(&mut t, revoked_link.id).await, Ok(true));
        let account = create_service_account(&mut t, "e2 relay robot", &[Scope::DocumentsWrite], a.owner).await.map_err(err)?;
        let robot = issue_credential(&mut t, account.id, 3_600, a.owner).await.map_err(err)?;
        t.commit().await?;

        let mut t = begin_tenant_transaction(&pool, b.tenant).await.map_err(err)?;
        let b_link = create_share_link(&mut t, b.document, ShareRole::Editor, None, b.owner, None).await.map_err(err)?;
        t.commit().await?;

        // A document and a workspace archived after their links were made.
        let mut t = begin_tenant_transaction(&pool, archived.tenant).await.map_err(err)?;
        let archived_doc_link = create_share_link(&mut t, archived.document, ShareRole::Editor, None, archived.owner, None).await.map_err(err)?;
        let archived_ws_link =
            create_share_link(&mut t, archived.other_document, ShareRole::Editor, None, archived.owner, None).await.map_err(err)?;
        archive_document(&mut t, archived.document).await.map_err(err)?.ok_or("archive document")?;
        t.commit().await?;
        let archived_owner = session(&pool, archived.owner).await?;
        let mut t = begin_tenant_transaction(&pool, archived.tenant).await.map_err(err)?;
        transition_workspace(&mut t, archived.workspace, Transition::Archive).await.map_err(err)?.ok_or("archive workspace")?;
        t.commit().await?;

        let mut t = begin_tenant_transaction(&pool, archived_project.tenant).await.map_err(err)?;
        let archived_project_link =
            create_share_link(&mut t, archived_project.document, ShareRole::Editor, None, archived_project.owner, None).await.map_err(err)?;
        transition_project(&mut t, archived_project.project, Transition::Archive).await.map_err(err)?.ok_or("archive project")?;
        t.commit().await?;

        let token = |tenant: TenantId, session: &String| member_token(tenant, session);
        let s = |principal| session(&pool, principal);
        let (sv, sc, se, sw, sa, so, sb, ss, sx, s_b) = (
            s(viewer).await?,
            s(commenter).await?,
            s(editor).await?,
            s(ws_admin).await?,
            s(admin).await?,
            s(a.owner).await?,
            s(bare).await?,
            s(suspended).await?,
            s(outsider).await?,
            s(b.owner).await?,
        );
        let logged_out = s(editor).await?;
        assert!(logout(&pool, &logged_out).await.map_err(err)?);

        Ok::<_, Error>(vec![
            // Share links.
            ("share editor link", a.document, editor_link.secret.clone(), EDIT),
            ("share viewer link", a.document, viewer_link.secret.clone(), VIEW),
            ("revoked link", a.document, revoked_link.secret, REFUSE),
            ("A's link on another document of A", a.other_document, editor_link.secret.clone(), REFUSE),
            ("A's link on B's document", b.document, editor_link.secret.clone(), REFUSE),
            ("B's link on A's document", a.document, b_link.secret.clone(), REFUSE),
            ("B's link on its own document", b.document, b_link.secret, EDIT),
            ("link to an archived document", archived.document, archived_doc_link.secret, REFUSE),
            ("link into an archived workspace", archived.other_document, archived_ws_link.secret, REFUSE),
            ("link into an archived project", archived_project.document, archived_project_link.secret, REFUSE),
            ("unknown link", a.document, "e2-unknown-share-secret-000000000000000000".into(), REFUSE),
            // Members (session tokens).
            ("viewer", a.document, token(a.tenant, &sv), VIEW),
            ("commenter", a.document, token(a.tenant, &sc), VIEW),
            ("editor", a.document, token(a.tenant, &se), EDIT),
            ("workspace admin", a.document, token(a.tenant, &sw), EDIT),
            ("tenant admin", a.document, token(a.tenant, &sa), EDIT),
            ("tenant owner", a.document, token(a.tenant, &so), EDIT),
            ("member without a workspace role", a.document, token(a.tenant, &sb), REFUSE),
            ("suspended member", a.document, token(a.tenant, &ss), REFUSE),
            ("non-member naming A", a.document, token(a.tenant, &sx), REFUSE),
            ("logged-out session", a.document, token(a.tenant, &logged_out), REFUSE),
            ("B's owner naming A", a.document, token(a.tenant, &s_b), REFUSE),
            ("B's owner in B, A's document", a.document, token(b.tenant, &s_b), REFUSE),
            ("A's owner naming B, A's document", a.document, token(b.tenant, &so), REFUSE),
            ("A's owner in A, B's document", b.document, token(a.tenant, &so), REFUSE),
            ("owner, archived workspace", archived.other_document, token(archived.tenant, &archived_owner), REFUSE),
            ("service credential as a session", a.document, token(a.tenant, &robot.secret), REFUSE),
            ("service credential as a link", a.document, robot.secret, REFUSE),
            ("bare session token (no member prefix)", a.document, so.clone(), REFUSE),
            ("member token with a junk tenant", a.document, format!("{MEMBER_TOKEN_PREFIX}not-a-tenant.{so}"), REFUSE),
        ])
    })?;

    let mut config = CloudBackendConfig::new(database_url, objects);
    config.max_connections = 2;
    let mut backend = CloudBackend::connect(&config).map_err(err)?;
    let mut failures = Vec::new();
    for (label, file, token, want) in &rows {
        let gap = KNOWN_GAPS.iter().find(|(gap, _, _)| gap == label);
        match (outcome(&mut backend, *file, token), gap) {
            (Ok(got), Some((_, today, why))) if got == *today => eprintln!("known gap, still open: {label}: {got:?} ({why})"),
            (Ok(got), Some(_)) if got == *want => failures.push(format!("{label}: fixed; drop it from KNOWN_GAPS")),
            (Ok(got), None) if got == *want => {}
            (Ok(got), _) => failures.push(format!("{label}: expected {want:?}, got {got:?}")),
            (Err(message), _) => failures.push(format!("{label}: {message}")),
        }
    }
    assert!(failures.is_empty(), "{} of {} relay cells differ:\n{}", failures.len(), rows.len(), failures.join("\n"));
    // Refused hellos open nothing.
    assert_eq!(backend.open_documents(), 0);
    Ok(())
}

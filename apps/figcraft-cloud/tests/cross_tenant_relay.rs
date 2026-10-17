//! S4-022 (realtime): the collaboration relay never admits a hello whose share link belongs to one
//! tenant into a document of another, against the dev PostgreSQL (fixture prefix d6; skipped
//! without FIGCRAFT_TEST_DATABASE_URL_FILE and FIGCRAFT_TEST_S3_DIR).
//!
//! A hello names its document by UUID (the `file`) and presents a share-link secret (the
//! `token`). Tenant A's link presented for tenant B's document, and B's link for A's, are both
//! refused with the same message as an unknown link, and nothing is opened or watched; each link
//! still admits to its own document. After A's link is revoked, a cross-tenant retry is refused
//! the same way.

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, ProjectId, TenantId, Uuid, WorkspaceId};
use figcraft_cloud_relay::collab_backend::{CloudBackend, CloudBackendConfig, REFUSED};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::metadata::{create_document, create_project, create_workspace};
use figcraft_cloud_storage::objects::ObjectStoreSettings;
use figcraft_cloud_storage::share_links::{ShareRole, create_share_link, revoke_share_link};
use figcraft_cloud_storage::tenants::{NewTenant, create_tenant_with_owner};
use figcraft_collab_net::hub::Backend;
use sqlx::postgres::{PgPool, PgPoolOptions};

type Error = Box<dyn std::error::Error>;
type TestResult = Result<(), Error>;

fn d6() -> Uuid {
    let mut bytes = *Uuid::new_v4().as_bytes();
    bytes[0] = 0xd6;
    Uuid::from_bytes(bytes)
}

/// A fresh tenant with one document and an editor link to it: (tenant, document, link id, secret).
async fn tenant_with_link(pool: &PgPool) -> Result<(TenantId, CloudDocumentId, Uuid, String), Error> {
    let owner = PrincipalId::try_from(d6()).map_err(|_| "nil")?;
    sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'd6 relay probe')")
        .bind(owner.to_string())
        .execute(pool)
        .await?;
    let tenant = TenantId::try_from(d6()).map_err(|_| "nil")?;
    let slug = format!("d6-{}", &d6().simple().to_string()[2..14]);
    let mut t = begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("{error:?}"))?;
    create_tenant_with_owner(&mut t, NewTenant { slug: &slug, name: "d6 relay", plan_id: "team", home_region: "local" }, owner)
        .await
        .map_err(|error| format!("{error:?}"))?;
    let workspace = WorkspaceId::try_from(d6()).map_err(|_| "nil")?;
    create_workspace(&mut t, workspace, "d6 relay", None).await?;
    let project = ProjectId::try_from(d6()).map_err(|_| "nil")?;
    create_project(&mut t, project, workspace, "d6 relay", None).await?;
    let document = CloudDocumentId::random();
    create_document(&mut t, document, workspace, project, "d6 relay doc").await?;
    let link = create_share_link(&mut t, document, ShareRole::Editor, None, owner, None).await.map_err(|error| format!("{error:?}"))?;
    t.commit().await?;
    Ok((tenant, document, link.id, link.secret))
}

#[test]
fn a_link_of_one_tenant_never_admits_to_another_tenants_document() -> TestResult {
    let (Ok(database_url_file), Ok(s3_directory)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR")) else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; cross-tenant relay probe skipped");
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
    let (pool, a, b) = runtime.block_on(async {
        let pool = PgPoolOptions::new().max_connections(2).connect(&database_url).await?;
        let a = tenant_with_link(&pool).await?;
        let b = tenant_with_link(&pool).await?;
        Ok::<_, Error>((pool, a, b))
    })?;
    let (a_tenant, a_document, a_link, a_secret) = a;
    let (_, b_document, _, b_secret) = b;

    let mut config = CloudBackendConfig::new(database_url, objects);
    config.max_connections = 2;
    let mut backend = CloudBackend::connect(&config).map_err(|error| format!("{error:?}"))?;
    let refused = Err(REFUSED.to_string());

    // Cross-tenant hellos are refused exactly like an unknown link, and open nothing.
    assert_eq!(backend.admit(&b_document.to_string(), &a_secret).map(|_| ()), refused);
    assert_eq!(backend.admit(&a_document.to_string(), &b_secret).map(|_| ()), refused);
    assert_eq!(backend.admit(&b_document.to_string(), "an-unknown-share-secret").map(|_| ()), refused);
    assert_eq!(backend.open_documents(), 0);
    assert!(backend.revocation_watch().is_empty());

    // Each link still admits to its own document, carrying its own link id.
    let admitted = backend.admit(&a_document.to_string(), &a_secret).map_err(|error| format!("A's own link refused: {error}"))?;
    assert_eq!(admitted.link, Some(a_link.to_string()));
    assert!(backend.admit(&b_document.to_string(), &b_secret).is_ok());
    assert_eq!(backend.revocation_watch().len(), 2);

    // Once A's link is revoked it opens nothing anywhere.
    runtime.block_on(async {
        let mut t = begin_tenant_transaction(&pool, a_tenant).await.map_err(|error| format!("{error:?}"))?;
        assert_eq!(revoke_share_link(&mut t, a_link).await, Ok(true));
        t.commit().await?;
        Ok::<_, Error>(())
    })?;
    assert_eq!(backend.admit(&a_document.to_string(), &a_secret).map(|_| ()), refused);
    assert_eq!(backend.admit(&b_document.to_string(), &a_secret).map(|_| ()), refused);
    Ok(())
}

//! Share links against the dev PostgreSQL (fixture prefix b1; skipped without
//! FIGCRAFT_TEST_DATABASE_URL_FILE). Fixtures are created idempotently by the runtime role and a
//! fresh document is used per run.

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, ProjectId, TenantId, Uuid, WorkspaceId};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::metadata::{archive_document, create_document};
use figcraft_cloud_storage::share_links::{
    SECRET_CHARS, ShareLinkError, ShareRole, create_share_link, resolve_share_link, revoke_share_link, share_token_hash,
};
use sqlx::postgres::{PgPool, PgPoolOptions};

const TENANT: &str = "b1000000-0000-4000-8000-000000000011";
const OTHER_TENANT: &str = "b1000000-0000-4000-8000-000000000012";
const ACTOR: &str = "b1000000-0000-4000-8000-000000000019";
const WORKSPACE: &str = "b1100000-0000-4000-8000-000000000011";
const PROJECT: &str = "b1200000-0000-4000-8000-000000000011";

type TestResult = Result<(), Box<dyn std::error::Error>>;

async fn pool() -> Result<Option<PgPool>, Box<dyn std::error::Error>> {
    let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; share link probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    let pool = PgPoolOptions::new().max_connections(4).connect(database_url.trim_end()).await?;
    for (tenant, slug) in [(TENANT, "b1-share-links"), (OTHER_TENANT, "b1-share-links-other")] {
        let mut transaction = begin_tenant_transaction(&pool, tenant.parse()?).await.map_err(|error| format!("{error:?}"))?;
        sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'b1 share link probe') ON CONFLICT DO NOTHING")
            .bind(ACTOR)
            .execute(transaction.connection())
            .await?;
        sqlx::query("INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $2, 'b1 share links', 'test', 'local') ON CONFLICT DO NOTHING")
            .bind(tenant)
            .bind(slug)
            .execute(transaction.connection())
            .await?;
        transaction.commit().await?;
    }
    let mut transaction = begin_tenant_transaction(&pool, TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
    sqlx::query("INSERT INTO workspaces (id, tenant_id, name) VALUES ($1::uuid, $2::uuid, 'b1 workspace') ON CONFLICT DO NOTHING")
        .bind(WORKSPACE)
        .bind(TENANT)
        .execute(transaction.connection())
        .await?;
    sqlx::query(
        "INSERT INTO projects (id, tenant_id, workspace_id, name) VALUES ($1::uuid, $2::uuid, $3::uuid, 'b1 project') ON CONFLICT DO NOTHING",
    )
    .bind(PROJECT)
    .bind(TENANT)
    .bind(WORKSPACE)
    .execute(transaction.connection())
    .await?;
    transaction.commit().await?;
    Ok(Some(pool))
}

async fn new_document(pool: &PgPool) -> Result<CloudDocumentId, Box<dyn std::error::Error>> {
    let mut transaction = begin_tenant_transaction(pool, TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
    let id = CloudDocumentId::random();
    let workspace: WorkspaceId = WORKSPACE.parse()?;
    let project: ProjectId = PROJECT.parse()?;
    create_document(&mut transaction, id, workspace, project, "b1 share link doc").await?;
    transaction.commit().await?;
    Ok(id)
}

async fn create(pool: &PgPool, document: CloudDocumentId, role: ShareRole) -> Result<(Uuid, String), Box<dyn std::error::Error>> {
    let mut transaction = begin_tenant_transaction(pool, TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
    let link = create_share_link(&mut transaction, document, role, Some("b1"), ACTOR.parse()?, None).await?;
    transaction.commit().await?;
    Ok((link.id, link.secret))
}

#[tokio::test]
async fn links_resolve_to_their_document_and_role_and_store_only_a_hash() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let document = new_document(&pool).await?;
    let (editor_id, editor) = create(&pool, document, ShareRole::Editor).await?;
    let (_, viewer) = create(&pool, document, ShareRole::Viewer).await?;
    assert_eq!(editor.len(), SECRET_CHARS);
    assert_ne!(editor, viewer);

    let resolved = resolve_share_link(&pool, &editor).await?;
    assert_eq!(resolved.id, editor_id);
    assert_eq!(resolved.document_id, document);
    assert_eq!(resolved.tenant_id, TENANT.parse::<TenantId>()?);
    assert_eq!(resolved.role, ShareRole::Editor);
    assert_eq!(resolved.created_by, ACTOR.parse::<PrincipalId>()?);
    assert_eq!(resolve_share_link(&pool, &viewer).await?.role, ShareRole::Viewer);

    let stored: Vec<u8> =
        sqlx::query_scalar("SELECT token_hash FROM share_links WHERE id = $1::uuid").bind(editor_id.to_string()).fetch_one(&pool).await?;
    assert_eq!(stored, share_token_hash(&editor).to_vec());
    let leaked: i64 = sqlx::query_scalar("SELECT count(*) FROM share_links WHERE encode(token_hash, 'escape') LIKE '%' || $1 || '%' OR label = $1")
        .bind(&editor)
        .fetch_one(&pool)
        .await?;
    assert_eq!(leaked, 0);
    Ok(())
}

#[tokio::test]
async fn unknown_malformed_revoked_expired_and_archived_links_are_refused_alike() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let document = new_document(&pool).await?;
    let (id, secret) = create(&pool, document, ShareRole::Editor).await?;

    // Unknown and malformed secrets.
    let mut wrong = secret.clone().into_bytes();
    if let Some(first) = wrong.first_mut() {
        *first = if *first == b'A' { b'B' } else { b'A' };
    }
    let wrong = String::from_utf8(wrong)?;
    for bad in [wrong.as_str(), "", "short", &"x".repeat(4096), &format!("{secret} ")] {
        assert_eq!(resolve_share_link(&pool, bad).await, Err(ShareLinkError::Refused));
    }

    // Revoked: once; a second revoke reports nothing to revoke.
    let mut transaction = begin_tenant_transaction(&pool, TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
    assert!(revoke_share_link(&mut transaction, id).await?);
    assert!(!revoke_share_link(&mut transaction, id).await?);
    transaction.commit().await?;
    assert_eq!(resolve_share_link(&pool, &secret).await, Err(ShareLinkError::Refused));

    // Another tenant can't revoke a link it doesn't own.
    let (other_id, other_secret) = create(&pool, document, ShareRole::Viewer).await?;
    let mut transaction = begin_tenant_transaction(&pool, OTHER_TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
    assert!(!revoke_share_link(&mut transaction, other_id).await?);
    transaction.commit().await?;
    assert!(resolve_share_link(&pool, &other_secret).await.is_ok());

    // Expired.
    let mut transaction = begin_tenant_transaction(&pool, TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
    let expiring = create_share_link(&mut transaction, document, ShareRole::Editor, None, ACTOR.parse()?, Some(3600)).await?;
    transaction.commit().await?;
    assert!(resolve_share_link(&pool, &expiring.secret).await.is_ok());
    sqlx::query("UPDATE share_links SET created_at = now() - interval '2 hours', expires_at = now() - interval '1 hour' WHERE id = $1::uuid")
        .bind(expiring.id.to_string())
        .execute(&pool)
        .await?;
    assert_eq!(resolve_share_link(&pool, &expiring.secret).await, Err(ShareLinkError::Refused));

    // A document that no longer accepts collaborators.
    let mut transaction = begin_tenant_transaction(&pool, TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
    archive_document(&mut transaction, document).await?;
    transaction.commit().await?;
    assert_eq!(resolve_share_link(&pool, &other_secret).await, Err(ShareLinkError::Refused));
    Ok(())
}

#[tokio::test]
async fn creation_validates_its_input_and_the_document() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let document = new_document(&pool).await?;
    let actor: PrincipalId = ACTOR.parse()?;
    let mut transaction = begin_tenant_transaction(&pool, TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
    let long = "x".repeat(256);
    for (label, lifetime) in [(Some(""), None), (Some(long.as_str()), None), (None, Some(0)), (None, Some(400 * 24 * 3600))] {
        assert_eq!(create_share_link(&mut transaction, document, ShareRole::Editor, label, actor, lifetime).await, Err(ShareLinkError::InvalidInput));
    }
    transaction.rollback().await?;
    // A document of another tenant (or none at all) can't be linked.
    let mut transaction = begin_tenant_transaction(&pool, OTHER_TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
    assert_eq!(create_share_link(&mut transaction, document, ShareRole::Editor, None, actor, None).await, Err(ShareLinkError::Constraint));
    transaction.rollback().await?;
    Ok(())
}

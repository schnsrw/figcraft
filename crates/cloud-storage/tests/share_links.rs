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

// S4-005 / S4-017 (fixture prefix c6): share-token exchange, listing, updating and the relay's
// revocation check.
mod c6 {
    use super::*;
    use figcraft_cloud_storage::share_links::{
        MAX_CHECKED_LINKS, MAX_SHARE_SESSION_SECS, ShareLinkUpdate, dead_share_links, exchange_share_token, list_share_links, update_share_link,
    };

    const TENANT: &str = "c6000000-0000-4000-8000-000000000011";
    const OTHER_TENANT: &str = "c6000000-0000-4000-8000-000000000012";
    const ACTOR: &str = "c6000000-0000-4000-8000-000000000019";
    const WORKSPACE: &str = "c6100000-0000-4000-8000-000000000011";
    const PROJECT: &str = "c6200000-0000-4000-8000-000000000011";

    async fn pool() -> Result<Option<PgPool>, Box<dyn std::error::Error>> {
        let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
            eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; c6 share link probe skipped");
            return Ok(None);
        };
        let database_url = std::fs::read_to_string(database_url_file)?;
        let pool = PgPoolOptions::new().max_connections(4).connect(database_url.trim_end()).await?;
        for (tenant, slug) in [(TENANT, "c6-share-links"), (OTHER_TENANT, "c6-share-links-other")] {
            let mut transaction = begin_tenant_transaction(&pool, tenant.parse()?).await.map_err(|error| format!("{error:?}"))?;
            sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'c6 share link probe') ON CONFLICT DO NOTHING")
                .bind(ACTOR)
                .execute(transaction.connection())
                .await?;
            sqlx::query(
                "INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $2, 'c6 share links', 'test', 'local') ON CONFLICT DO NOTHING",
            )
            .bind(tenant)
            .bind(slug)
            .execute(transaction.connection())
            .await?;
            transaction.commit().await?;
        }
        let mut transaction = begin_tenant_transaction(&pool, TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
        sqlx::query("INSERT INTO workspaces (id, tenant_id, name) VALUES ($1::uuid, $2::uuid, 'c6 workspace') ON CONFLICT DO NOTHING")
            .bind(WORKSPACE)
            .bind(TENANT)
            .execute(transaction.connection())
            .await?;
        sqlx::query(
            "INSERT INTO projects (id, tenant_id, workspace_id, name) VALUES ($1::uuid, $2::uuid, $3::uuid, 'c6 project') ON CONFLICT DO NOTHING",
        )
        .bind(PROJECT)
        .bind(TENANT)
        .bind(WORKSPACE)
        .execute(transaction.connection())
        .await?;
        transaction.commit().await?;
        Ok(Some(pool))
    }

    async fn document(pool: &PgPool) -> Result<CloudDocumentId, Box<dyn std::error::Error>> {
        let mut transaction = begin_tenant_transaction(pool, TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
        let id = CloudDocumentId::random();
        let workspace: WorkspaceId = WORKSPACE.parse()?;
        let project: ProjectId = PROJECT.parse()?;
        create_document(&mut transaction, id, workspace, project, "c6 share link doc").await?;
        transaction.commit().await?;
        Ok(id)
    }

    async fn link(
        pool: &PgPool,
        document: CloudDocumentId,
        role: ShareRole,
        label: Option<&str>,
        expires_in: Option<u64>,
    ) -> Result<(Uuid, String), Box<dyn std::error::Error>> {
        let mut transaction = begin_tenant_transaction(pool, TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
        let link = create_share_link(&mut transaction, document, role, label, ACTOR.parse()?, expires_in).await?;
        transaction.commit().await?;
        Ok((link.id, link.secret))
    }

    async fn now(pool: &PgPool) -> Result<i64, Box<dyn std::error::Error>> {
        Ok(sqlx::query_scalar("SELECT floor(extract(epoch FROM now()))::bigint").fetch_one(pool).await?)
    }

    async fn revoke(pool: &PgPool, id: Uuid) -> TestResult {
        let mut transaction = begin_tenant_transaction(pool, TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
        assert!(revoke_share_link(&mut transaction, id).await?);
        transaction.commit().await?;
        Ok(())
    }

    #[tokio::test]
    async fn exchange_grants_a_session_bounded_by_the_link_and_refuses_like_resolution() -> TestResult {
        let Some(pool) = pool().await? else {
            return Ok(());
        };
        let document = document(&pool).await?;
        let (forever_id, forever) = link(&pool, document, ShareRole::Editor, None, None).await?;
        let (_, brief) = link(&pool, document, ShareRole::Viewer, None, Some(120)).await?;
        let start = now(&pool).await?;
        let max = i64::try_from(MAX_SHARE_SESSION_SECS)?;

        let grant = exchange_share_token(&pool, &forever).await?;
        assert_eq!((grant.link_id, grant.document_id, grant.role), (forever_id, document, ShareRole::Editor));
        assert_eq!(grant.tenant_id, TENANT.parse::<TenantId>()?);
        assert_eq!(grant.created_by, ACTOR.parse::<PrincipalId>()?);
        assert_eq!(grant.link_expires_at, None);
        assert!((start + max - 2..=start + max + 2).contains(&grant.session_expires_at), "{grant:?}");
        // The grant prints nothing secret.
        assert!(!format!("{grant:?}").contains(&forever));

        // A link expiring sooner than a session ends the session with it.
        let grant = exchange_share_token(&pool, &brief).await?;
        assert_eq!(grant.role, ShareRole::Viewer);
        let expires = grant.link_expires_at.ok_or("brief link has an expiry")?;
        assert!((start + 118..=start + 122).contains(&expires));
        assert_eq!(grant.session_expires_at, expires);

        // Resolution agrees with the exchange.
        assert_eq!(resolve_share_link(&pool, &forever).await?.id, forever_id);

        // Revoked, malformed and archived-document links are refused alike.
        revoke(&pool, forever_id).await?;
        for bad in [forever.as_str(), "", "nope"] {
            assert_eq!(exchange_share_token(&pool, bad).await, Err(ShareLinkError::Refused));
        }
        let mut transaction = begin_tenant_transaction(&pool, TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
        archive_document(&mut transaction, document).await?;
        transaction.commit().await?;
        assert_eq!(exchange_share_token(&pool, &brief).await, Err(ShareLinkError::Refused));
        Ok(())
    }

    #[tokio::test]
    async fn links_are_listed_and_updated_within_their_tenant_only() -> TestResult {
        let Some(pool) = pool().await? else {
            return Ok(());
        };
        let document = document(&pool).await?;
        let (first, _) = link(&pool, document, ShareRole::Editor, Some("c6 first"), None).await?;
        let (second, second_secret) = link(&pool, document, ShareRole::Viewer, None, Some(3600)).await?;
        let (third, _) = link(&pool, document, ShareRole::Viewer, Some("c6 third"), None).await?;
        revoke(&pool, third).await?;
        let start = now(&pool).await?;

        let mut transaction = begin_tenant_transaction(&pool, TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
        let listed = list_share_links(&mut transaction, document).await?;
        let ids: Vec<Uuid> = listed.iter().map(|record| record.id).collect();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&first) && ids.contains(&second) && ids.contains(&third));
        assert!(listed.windows(2).all(|pair| pair[0].created_at >= pair[1].created_at));
        let by_id = |id: Uuid| listed.iter().find(|record| record.id == id).cloned().ok_or("listed");
        assert_eq!(by_id(first)?.label.as_deref(), Some("c6 first"));
        assert_eq!(by_id(first)?.role, ShareRole::Editor);
        assert!(by_id(first)?.live_at(start) && by_id(second)?.live_at(start));
        assert!(by_id(third)?.revoked_at.is_some() && !by_id(third)?.live_at(start));

        // Label and expiry change; unchanged fields stay.
        let update = ShareLinkUpdate { label: Some(Some("c6 renamed")), expires_in_secs: Some(Some(600)) };
        let updated = update_share_link(&mut transaction, second, &update).await?.ok_or("second is live")?;
        assert_eq!(updated.label.as_deref(), Some("c6 renamed"));
        let expires = updated.expires_at.ok_or("expiry set")?;
        assert!((start + 598..=start + 602).contains(&expires));
        let cleared = update_share_link(&mut transaction, second, &ShareLinkUpdate { label: Some(None), expires_in_secs: Some(None) })
            .await?
            .ok_or("second is live")?;
        assert_eq!((cleared.label, cleared.expires_at), (None, None));
        let unchanged = update_share_link(&mut transaction, first, &ShareLinkUpdate::default()).await?.ok_or("first is live")?;
        assert_eq!(unchanged.label.as_deref(), Some("c6 first"));
        // A revoked link can't be changed (nor brought back).
        assert_eq!(update_share_link(&mut transaction, third, &update).await?, None);
        // Bad input.
        let long = "x".repeat(256);
        for bad in [
            ShareLinkUpdate { label: Some(Some("")), expires_in_secs: None },
            ShareLinkUpdate { label: Some(Some(long.as_str())), expires_in_secs: None },
            ShareLinkUpdate { label: None, expires_in_secs: Some(Some(0)) },
            ShareLinkUpdate { label: None, expires_in_secs: Some(Some(400 * 24 * 3600)) },
        ] {
            assert_eq!(update_share_link(&mut transaction, first, &bad).await, Err(ShareLinkError::InvalidInput));
        }
        transaction.commit().await?;
        assert!(exchange_share_token(&pool, &second_secret).await?.link_expires_at.is_none());

        // Another tenant sees and changes nothing.
        let mut transaction = begin_tenant_transaction(&pool, OTHER_TENANT.parse()?).await.map_err(|error| format!("{error:?}"))?;
        assert!(list_share_links(&mut transaction, document).await?.is_empty());
        assert_eq!(update_share_link(&mut transaction, first, &update).await?, None);
        transaction.rollback().await?;
        Ok(())
    }

    #[tokio::test]
    async fn dead_links_are_exactly_those_that_no_longer_open_anything() -> TestResult {
        let Some(pool) = pool().await? else {
            return Ok(());
        };
        let tenant: TenantId = TENANT.parse()?;
        let other: TenantId = OTHER_TENANT.parse()?;
        let active = document(&pool).await?;
        let archived = document(&pool).await?;
        let (live, _) = link(&pool, active, ShareRole::Editor, None, None).await?;
        let (revoked, _) = link(&pool, active, ShareRole::Editor, None, None).await?;
        let (expired, _) = link(&pool, active, ShareRole::Viewer, None, Some(3600)).await?;
        let (on_archived, _) = link(&pool, archived, ShareRole::Viewer, None, None).await?;
        revoke(&pool, revoked).await?;
        sqlx::query("UPDATE share_links SET created_at = now() - interval '2 hours', expires_at = now() - interval '1 hour' WHERE id = $1::uuid")
            .bind(expired.to_string())
            .execute(&pool)
            .await?;
        let mut transaction = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
        archive_document(&mut transaction, archived).await?;
        transaction.commit().await?;
        let unknown = Uuid::new_v4();

        let asked = [(tenant, live), (tenant, revoked), (tenant, expired), (tenant, on_archived), (tenant, unknown), (other, live)];
        let dead = dead_share_links(&pool, &asked).await?;
        let expected = [(tenant, revoked), (tenant, expired), (tenant, on_archived), (tenant, unknown), (other, live)];
        assert_eq!(dead.len(), expected.len(), "{dead:?}");
        assert!(expected.iter().all(|pair| dead.contains(pair)), "{dead:?}");
        assert!(dead_share_links(&pool, &[(tenant, live)]).await?.is_empty());
        assert!(dead_share_links(&pool, &[]).await?.is_empty());
        let too_many = vec![(tenant, live); MAX_CHECKED_LINKS + 1];
        assert_eq!(dead_share_links(&pool, &too_many).await, Err(ShareLinkError::InvalidInput));
        Ok(())
    }

    #[tokio::test]
    async fn a_tenant_that_is_no_longer_active_closes_its_share_links() -> TestResult {
        const SUSPENDED: &str = "c6000000-0000-4000-8000-000000000013";
        const SUSPENDED_WORKSPACE: &str = "c6100000-0000-4000-8000-000000000013";
        const SUSPENDED_PROJECT: &str = "c6200000-0000-4000-8000-000000000013";
        let Some(pool) = pool().await? else {
            return Ok(());
        };
        let tenant: TenantId = SUSPENDED.parse()?;
        let mut transaction = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
        let statements = [
            "INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, 'c6-share-links-suspended', 'c6 suspended', 'test', 'local') \
             ON CONFLICT (id) DO UPDATE SET state = 'active'",
            "INSERT INTO workspaces (id, tenant_id, name) VALUES ($2::uuid, $1::uuid, 'c6 suspended') ON CONFLICT DO NOTHING",
            "INSERT INTO projects (id, tenant_id, workspace_id, name) VALUES ($3::uuid, $1::uuid, $2::uuid, 'c6 suspended') ON CONFLICT DO NOTHING",
        ];
        for statement in statements {
            sqlx::query(statement).bind(SUSPENDED).bind(SUSPENDED_WORKSPACE).bind(SUSPENDED_PROJECT).execute(transaction.connection()).await?;
        }
        let document = CloudDocumentId::random();
        create_document(&mut transaction, document, SUSPENDED_WORKSPACE.parse()?, SUSPENDED_PROJECT.parse()?, "c6 suspended doc").await?;
        let link = create_share_link(&mut transaction, document, ShareRole::Editor, None, ACTOR.parse()?, None).await?;
        transaction.commit().await?;
        assert_eq!(exchange_share_token(&pool, &link.secret).await?.tenant_id, tenant);
        assert!(dead_share_links(&pool, &[(tenant, link.id)]).await?.is_empty());

        let mut transaction = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("{error:?}"))?;
        sqlx::query("UPDATE tenants SET state = 'suspended' WHERE id = $1::uuid").bind(SUSPENDED).execute(transaction.connection()).await?;
        transaction.commit().await?;
        assert_eq!(exchange_share_token(&pool, &link.secret).await, Err(ShareLinkError::Refused));
        assert_eq!(resolve_share_link(&pool, &link.secret).await.map(|link| link.id), Err(ShareLinkError::Refused));
        assert_eq!(dead_share_links(&pool, &[(tenant, link.id)]).await?, vec![(tenant, link.id)]);
        Ok(())
    }
}

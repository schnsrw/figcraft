//! S6-002 host external id mappings against PostgreSQL (FIGCRAFT_TEST_DATABASE_URL_FILE).
//! Fixture tenants are random, so runs are independent.

use figcraft_cloud_domain::{TenantId, Uuid};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::host_mappings::{
    HostMappingError, MAX_EXTERNAL_ID_BYTES, MappingKind, external_ids_for_internal, map_external_resource, resolve_external_resource,
    unmap_external_resource,
};
use figcraft_cloud_storage::host_systems::{HostRole, NewHostSystem, register_host_system};
use sqlx::postgres::{PgPool, PgPoolOptions};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct Fixture {
    tenant: TenantId,
    workspace: Uuid,
    project: Uuid,
    document: Uuid,
    principal: Uuid,
}

async fn pool() -> TestResult<Option<PgPool>> {
    let Ok(file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; host mapping probe skipped");
        return Ok(None);
    };
    let url = std::fs::read_to_string(file)?;
    Ok(Some(PgPoolOptions::new().max_connections(4).connect(url.trim_end()).await?))
}

async fn fixture(pool: &PgPool) -> TestResult<Fixture> {
    let f = Fixture {
        tenant: TenantId::random(),
        workspace: Uuid::new_v4(),
        project: Uuid::new_v4(),
        document: Uuid::new_v4(),
        principal: Uuid::new_v4(),
    };
    let mut tx = begin_tenant_transaction(pool, f.tenant).await.map_err(|e| format!("{e:?}"))?;
    let t = f.tenant.to_string();
    sqlx::query("INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $2, 'map probe', 'test', 'local')")
        .bind(&t)
        .bind(format!("map-{}", f.tenant.as_uuid().simple()))
        .execute(tx.connection())
        .await?;
    sqlx::query("INSERT INTO workspaces (id, tenant_id, name) VALUES ($1::uuid, $2::uuid, 'w')")
        .bind(f.workspace.to_string())
        .bind(&t)
        .execute(tx.connection())
        .await?;
    sqlx::query("INSERT INTO projects (id, tenant_id, workspace_id, name) VALUES ($1::uuid, $2::uuid, $3::uuid, 'p')")
        .bind(f.project.to_string())
        .bind(&t)
        .bind(f.workspace.to_string())
        .execute(tx.connection())
        .await?;
    sqlx::query("INSERT INTO documents (id, tenant_id, workspace_id, project_id, name) VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 'd')")
        .bind(f.document.to_string())
        .bind(&t)
        .bind(f.workspace.to_string())
        .bind(f.project.to_string())
        .execute(tx.connection())
        .await?;
    sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'map probe')")
        .bind(f.principal.to_string())
        .execute(tx.connection())
        .await?;
    sqlx::query("INSERT INTO tenant_members (tenant_id, principal_id, role, state) VALUES ($1::uuid, $2::uuid, 'viewer', 'active')")
        .bind(&t)
        .bind(f.principal.to_string())
        .execute(tx.connection())
        .await?;
    for (system, issuer) in [("host-a", "a"), ("host-b", "b")] {
        register_host_system(
            &mut tx,
            &NewHostSystem {
                system_id: system.into(),
                issuer: format!("https://{issuer}-{}.example", Uuid::new_v4().simple()),
                audiences: vec!["figcraft".into()],
                allowed_origins: vec![],
                max_role: HostRole::Editor,
                allow_insecure_localhost: false,
            },
        )
        .await?;
    }
    tx.commit().await?;
    Ok(f)
}

#[tokio::test]
async fn maps_resolves_reverses_and_unmaps_every_kind() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let f = fixture(&pool).await?;
    let mut tx = begin_tenant_transaction(&pool, f.tenant).await.map_err(|e| format!("{e:?}"))?;
    let cases = [
        (MappingKind::Tenant, f.tenant.as_uuid()),
        (MappingKind::Workspace, f.workspace),
        (MappingKind::Project, f.project),
        (MappingKind::Document, f.document),
        (MappingKind::Principal, f.principal),
    ];
    for (kind, internal) in cases {
        let created = map_external_resource(&mut tx, "host-a", kind, "ext-1", internal).await?;
        assert_eq!((created.internal_id, created.system_id.as_str()), (internal, "host-a"));
        assert_eq!(resolve_external_resource(&mut tx, "host-a", kind, "ext-1").await?, Some(internal));
        // Unique per host: host-b has no such mapping.
        assert_eq!(resolve_external_resource(&mut tx, "host-b", kind, "ext-1").await?, None);
        assert_eq!(external_ids_for_internal(&mut tx, "host-a", kind, internal).await?, vec!["ext-1".to_owned()]);
    }
    map_external_resource(&mut tx, "host-b", MappingKind::Document, "ext-1", f.document).await?;
    assert_eq!(resolve_external_resource(&mut tx, "host-b", MappingKind::Document, "ext-1").await?, Some(f.document));
    assert!(unmap_external_resource(&mut tx, "host-a", MappingKind::Document, "ext-1").await?);
    assert!(!unmap_external_resource(&mut tx, "host-a", MappingKind::Document, "ext-1").await?);
    assert_eq!(resolve_external_resource(&mut tx, "host-a", MappingKind::Document, "ext-1").await?, None);
    assert_eq!(resolve_external_resource(&mut tx, "host-b", MappingKind::Document, "ext-1").await?, Some(f.document));
    tx.commit().await?;
    Ok(())
}

#[tokio::test]
async fn refuses_bad_ids_unknown_hosts_duplicates_and_foreign_resources() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let a = fixture(&pool).await?;
    let b = fixture(&pool).await?;
    let mut tx = begin_tenant_transaction(&pool, a.tenant).await.map_err(|e| format!("{e:?}"))?;
    for bad in ["", "a\nb", &"x".repeat(MAX_EXTERNAL_ID_BYTES + 1)] {
        assert_eq!(
            map_external_resource(&mut tx, "host-a", MappingKind::Document, bad, a.document).await.err(),
            Some(HostMappingError::InvalidInput)
        );
    }
    assert_eq!(map_external_resource(&mut tx, "host-a", MappingKind::Document, "e", Uuid::nil()).await.err(), Some(HostMappingError::InvalidInput));
    assert_eq!(map_external_resource(&mut tx, "nobody", MappingKind::Document, "e", a.document).await.err(), Some(HostMappingError::NotFound));
    // Cross-tenant: tenant B's resources (and tenant B itself) are not mappable from tenant A.
    for (kind, internal) in [
        (MappingKind::Tenant, b.tenant.as_uuid()),
        (MappingKind::Workspace, b.workspace),
        (MappingKind::Project, b.project),
        (MappingKind::Document, b.document),
        (MappingKind::Principal, b.principal),
        (MappingKind::Document, Uuid::new_v4()),
    ] {
        assert_eq!(map_external_resource(&mut tx, "host-a", kind, "e", internal).await.err(), Some(HostMappingError::NotFound));
    }
    map_external_resource(&mut tx, "host-a", MappingKind::Document, "dup", a.document).await?;
    tx.commit().await?;

    let mut tx = begin_tenant_transaction(&pool, a.tenant).await.map_err(|e| format!("{e:?}"))?;
    assert_eq!(map_external_resource(&mut tx, "host-a", MappingKind::Document, "dup", a.document).await.err(), Some(HostMappingError::Conflict));
    tx.rollback().await?;

    // Tenant B cannot see or remove tenant A's mapping, even naming the same host id.
    let mut tx = begin_tenant_transaction(&pool, b.tenant).await.map_err(|e| format!("{e:?}"))?;
    assert_eq!(resolve_external_resource(&mut tx, "host-a", MappingKind::Document, "dup").await?, None);
    assert!(!unmap_external_resource(&mut tx, "host-a", MappingKind::Document, "dup").await?);
    assert!(external_ids_for_internal(&mut tx, "host-a", MappingKind::Document, a.document).await?.is_empty());
    tx.rollback().await?;
    Ok(())
}

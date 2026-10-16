use figcraft_cloud_domain::{PrincipalId, TenantId, Uuid};
use figcraft_cloud_storage::audit::{RepositoryError, record_audit_event};
use figcraft_cloud_storage::begin_tenant_transaction;
use sqlx::postgres::PgPoolOptions;

#[tokio::test]
async fn audit_repository_appends_events_and_rejects_invalid_outcomes() -> Result<(), Box<dyn std::error::Error>> {
    let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; PostgreSQL audit repository probe skipped");
        return Ok(());
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    let pool = PgPoolOptions::new().max_connections(2).connect(database_url.trim_end()).await?;
    let tenant: TenantId = "92000000-0000-4000-8000-000000000001".parse()?;
    let principal: PrincipalId = "92000000-0000-4000-8000-000000000002".parse()?;
    let empty_metadata = serde_json::json!({});

    let mut scoped = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;

    // A succeeded event recorded by a real actor returns the same id the caller supplied.
    let actor_event = Uuid::new_v4();
    let returned = record_audit_event(
        &mut scoped,
        actor_event,
        Some(principal.as_uuid()),
        "document.create",
        "document",
        Some(Uuid::new_v4()),
        Some(Uuid::new_v4()),
        Some("203.0.113.7"),
        "succeeded",
        &serde_json::json!({"name": "New document"}),
    )
    .await?;
    assert_eq!(returned, actor_event);

    // A system/job-initiated action has no human actor.
    let system_event = Uuid::new_v4();
    let returned_system =
        record_audit_event(&mut scoped, system_event, None, "job.retention_sweep", "tenant", None, None, None, "succeeded", &empty_metadata).await?;
    assert_eq!(returned_system, system_event);

    // `resource_id` and `request_id` are plain nullable UUID columns with no foreign key, so any
    // UUID value is accepted when both are present together.
    let paired_event = Uuid::new_v4();
    let returned_paired = record_audit_event(
        &mut scoped,
        paired_event,
        Some(principal.as_uuid()),
        "document.archive",
        "document",
        Some(Uuid::new_v4()),
        Some(Uuid::new_v4()),
        Some("198.51.100.23/32"),
        "denied",
        &empty_metadata,
    )
    .await?;
    assert_eq!(returned_paired, paired_event);

    scoped.commit().await?;

    // A constraint violation aborts its Postgres transaction, so the invalid-outcome check gets
    // its own short-lived transaction rather than sharing one with the successful calls above.
    let mut invalid_outcome = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    assert_eq!(
        record_audit_event(
            &mut invalid_outcome,
            Uuid::new_v4(),
            Some(principal.as_uuid()),
            "document.create",
            "document",
            None,
            None,
            None,
            "maybe",
            &empty_metadata,
        )
        .await,
        Err(RepositoryError::Constraint)
    );
    invalid_outcome.rollback().await?;

    pool.close().await;
    Ok(())
}

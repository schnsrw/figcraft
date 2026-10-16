//! S3 follow-up: document recovery quarantine against PostgreSQL.
//!
//! A document whose log is broken (a corrupt event, a missing event, or a log that does not replay
//! over the base) is quarantined by the failed open, then refused by every later open and recover
//! until an operator clears it; clearing writes an audit event and re-allows opening. Logs are
//! broken with the administrator connection (the runtime role cannot update or delete events).
//! The fixture is `deploy/saas/quarantine-preflight.sql`; documents are fresh per run.

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, ProjectId, TenantId, WorkspaceId};
use figcraft_cloud_relay::{DurableRoom, RoomError};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::events::EventRepositoryError;
use figcraft_cloud_storage::metadata::create_document;
use figcraft_cloud_storage::quarantine::{
    QUARANTINE_CLEARED_ACTION, QuarantineChange, QuarantineError, QuarantineReason, clear_quarantine, is_quarantined, quarantine_document,
    quarantine_reason,
};
use figcraft_collab::message::{Access, ClientMsg};
use figcraft_collab::{Change, Client, Json, Limits, Prop};
use figcraft_doc::{Document, NodeId, NodeKind};
use sqlx::postgres::{PgPool, PgPoolOptions};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const TENANT: &str = "af000000-0000-4000-8000-000000000001";
const OTHER_TENANT: &str = "af000000-0000-4000-8000-000000000002";
const OPERATOR: &str = "af000000-0000-4000-8000-000000000009";
const WORKSPACE: &str = "af100000-0000-4000-8000-000000000001";
const PROJECT: &str = "af200000-0000-4000-8000-000000000001";

/// The runtime pool and the administrator pool.
async fn pools() -> TestResult<Option<(PgPool, PgPool)>> {
    let (Ok(runtime), Ok(admin)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE")) else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE is unset; quarantine probe skipped");
        return Ok(None);
    };
    let connect = |url: String| async move { PgPoolOptions::new().max_connections(2).connect(url.trim_end()).await };
    Ok(Some((connect(std::fs::read_to_string(runtime)?).await?, connect(std::fs::read_to_string(admin)?).await?)))
}

fn base() -> TestResult<(Document, NodeId)> {
    let mut document = Document::new("Shared");
    let page = document.first_page().ok_or("no page")?;
    let frame = document.create(page, NodeKind::Frame, "Frame", 0.0, 0.0, 100.0, 100.0)?;
    Ok((document, frame))
}

fn tenant() -> TestResult<TenantId> {
    Ok(TENANT.parse()?)
}

async fn open(pool: &PgPool, document: CloudDocumentId) -> Result<DurableRoom, RoomError> {
    let (base, _) = base().map_err(|_| RoomError::Replay)?;
    let tenant = tenant().map_err(|_| RoomError::Begin)?;
    DurableRoom::open(pool.clone(), tenant, document, base, "", Limits::default()).await
}

/// A fresh document with three committed renames of its frame.
async fn document_with_log(pool: &PgPool) -> TestResult<(CloudDocumentId, NodeId)> {
    let mut transaction = begin_tenant_transaction(pool, tenant()?).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let id = CloudDocumentId::random();
    let workspace: WorkspaceId = WORKSPACE.parse()?;
    let project: ProjectId = PROJECT.parse()?;
    create_document(&mut transaction, id, workspace, project, "Quarantine probe").await?;
    transaction.commit().await?;
    let (_, frame) = base()?;
    let mut room = open(pool, id).await?;
    let ClientMsg::Hello(hello) = Client::hello("f", "", "Ada", "#FF8800") else {
        return Err("not a hello".into());
    };
    let (client, _) = room.join(hello, Access::Edit)?.map_err(|refusal| format!("join refused: {refusal:?}"))?;
    let actor: PrincipalId = OPERATOR.parse()?;
    for (batch_id, name) in (1..).zip(["One", "Two", "Three"]) {
        let changes = vec![Change::Set { id: frame, prop: Prop::Name, value: Json::of(name)? }];
        room.receive(client, actor, ClientMsg::Changes { base_seq: room.server().seq(), batch_id, changes }).await?;
    }
    Ok((id, frame))
}

async fn reason(pool: &PgPool, document: CloudDocumentId) -> TestResult<Option<QuarantineReason>> {
    let mut transaction = begin_tenant_transaction(pool, tenant()?).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let reason = quarantine_reason(&mut transaction, document).await?;
    transaction.rollback().await?;
    Ok(reason)
}

async fn clear(pool: &PgPool, document: CloudDocumentId) -> TestResult<QuarantineChange> {
    let mut transaction = begin_tenant_transaction(pool, tenant()?).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let change = clear_quarantine(&mut transaction, document, OPERATOR.parse()?).await?;
    transaction.commit().await?;
    Ok(change)
}

async fn cleared_audits(admin: &PgPool, document: CloudDocumentId) -> TestResult<Vec<(String, String)>> {
    Ok(sqlx::query_as(concat!(
        "SELECT actor_principal_id::text, metadata->>'reason' FROM audit_events ",
        "WHERE tenant_id = $1::uuid AND resource_id = $2::uuid AND action = $3 ORDER BY created_at, id"
    ))
    .bind(TENANT)
    .bind(document.to_string())
    .bind(QUARANTINE_CLEARED_ACTION)
    .fetch_all(admin)
    .await?)
}

/// A refused open must not claim the document: its fencing token stays where it was.
async fn fencing_token(admin: &PgPool, document: CloudDocumentId) -> TestResult<i64> {
    Ok(sqlx::query_scalar("SELECT fencing_token FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(TENANT)
        .bind(document.to_string())
        .fetch_one(admin)
        .await?)
}

#[tokio::test]
async fn a_corrupt_event_quarantines_the_document_until_an_operator_clears_it() -> TestResult {
    let Some((pool, admin)) = pools().await? else {
        return Ok(());
    };
    let (document, frame) = document_with_log(&pool).await?;
    assert_eq!(reason(&pool, document).await?, None);

    // Flip a payload byte of event 2: its checksum no longer matches.
    let original: Vec<u8> =
        sqlx::query_scalar("SELECT payload FROM document_events WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND seq = 2")
            .bind(TENANT)
            .bind(document.to_string())
            .fetch_one(&admin)
            .await?;
    let mut broken = original.clone();
    *broken.get_mut(0).ok_or("empty payload")? ^= 0x5a;
    let set_payload = "UPDATE document_events SET payload = $3 WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND seq = 2";
    sqlx::query(set_payload).bind(TENANT).bind(document.to_string()).bind(&broken).execute(&admin).await?;

    assert!(matches!(open(&pool, document).await, Err(RoomError::Open(EventRepositoryError::Corrupt))));
    assert_eq!(reason(&pool, document).await?, Some(QuarantineReason::LogCorrupt));
    let token = fencing_token(&admin, document).await?;
    assert!(matches!(open(&pool, document).await, Err(RoomError::Quarantined)));
    assert_eq!(fencing_token(&admin, document).await?, token, "a refused open claims nothing");

    // Repair the event; the document stays refused until the quarantine is cleared.
    sqlx::query(set_payload).bind(TENANT).bind(document.to_string()).bind(&original).execute(&admin).await?;
    assert!(matches!(open(&pool, document).await, Err(RoomError::Quarantined)));
    assert_eq!(clear(&pool, document).await?, QuarantineChange::Applied);
    assert_eq!(clear(&pool, document).await?, QuarantineChange::AlreadyApplied);
    assert_eq!(cleared_audits(&admin, document).await?, vec![(OPERATOR.to_string(), "log_corrupt".to_string())]);
    let room = open(&pool, document).await?;
    assert_eq!(room.server().seq(), 3);
    assert_eq!(room.server().document().node(frame).map(|node| node.name.as_str()), Some("Three"));
    pool.close().await;
    admin.close().await;
    Ok(())
}

#[tokio::test]
async fn a_missing_event_is_a_log_gap_and_clearing_without_repair_quarantines_again() -> TestResult {
    let Some((pool, admin)) = pools().await? else {
        return Ok(());
    };
    for missing in [2, 3] {
        let (document, _) = document_with_log(&pool).await?;
        sqlx::query("DELETE FROM document_events WHERE tenant_id = $1::uuid AND document_id = $2::uuid AND seq = $3")
            .bind(TENANT)
            .bind(document.to_string())
            .bind(missing)
            .execute(&admin)
            .await?;
        assert!(matches!(open(&pool, document).await, Err(RoomError::Open(EventRepositoryError::Corrupt))), "missing {missing}");
        assert_eq!(reason(&pool, document).await?, Some(QuarantineReason::LogGap), "missing {missing}");
        assert!(matches!(open(&pool, document).await, Err(RoomError::Quarantined)));

        // Clearing without repairing lets the next open diagnose the same gap again.
        assert_eq!(clear(&pool, document).await?, QuarantineChange::Applied);
        assert!(matches!(open(&pool, document).await, Err(RoomError::Open(EventRepositoryError::Corrupt))));
        assert_eq!(reason(&pool, document).await?, Some(QuarantineReason::LogGap));
        assert_eq!(cleared_audits(&admin, document).await?.len(), 1);
    }
    pool.close().await;
    admin.close().await;
    Ok(())
}

#[tokio::test]
async fn a_log_that_does_not_replay_quarantines_and_the_repository_is_idempotent() -> TestResult {
    let Some((pool, admin)) = pools().await? else {
        return Ok(());
    };
    let (document, _) = document_with_log(&pool).await?;
    let wrong_base = Document::new("Without the frame");
    assert!(matches!(DurableRoom::open(pool.clone(), tenant()?, document, wrong_base, "", Limits::default()).await, Err(RoomError::Replay)));
    assert_eq!(reason(&pool, document).await?, Some(QuarantineReason::ReplayFailed));
    assert!(matches!(open(&pool, document).await, Err(RoomError::Quarantined)));

    // Quarantine keeps its first reason; the other tenant sees no such document.
    let mut transaction = begin_tenant_transaction(&pool, tenant()?).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    assert!(is_quarantined(&mut transaction, document).await?);
    assert_eq!(quarantine_document(&mut transaction, document, QuarantineReason::Operator).await?, QuarantineChange::AlreadyApplied);
    assert_eq!(quarantine_reason(&mut transaction, document).await?, Some(QuarantineReason::ReplayFailed));
    let unknown = CloudDocumentId::random();
    assert_eq!(quarantine_document(&mut transaction, unknown, QuarantineReason::Operator).await, Err(QuarantineError::NotFound));
    assert_eq!(is_quarantined(&mut transaction, unknown).await, Err(QuarantineError::NotFound));
    assert_eq!(clear_quarantine(&mut transaction, unknown, OPERATOR.parse()?).await, Err(QuarantineError::NotFound));
    transaction.rollback().await?;
    let mut other = begin_tenant_transaction(&pool, OTHER_TENANT.parse()?).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    assert_eq!(quarantine_reason(&mut other, document).await, Err(QuarantineError::NotFound));
    assert_eq!(clear_quarantine(&mut other, document, OPERATOR.parse()?).await, Err(QuarantineError::NotFound));
    other.rollback().await?;
    assert!(matches!(
        DurableRoom::open(pool.clone(), OTHER_TENANT.parse()?, document, base()?.0, "", Limits::default()).await,
        Err(RoomError::Open(EventRepositoryError::NotFound))
    ));

    // Cleared, the document opens over the right base; an operator can quarantine it by hand.
    assert_eq!(clear(&pool, document).await?, QuarantineChange::Applied);
    assert_eq!(cleared_audits(&admin, document).await?, vec![(OPERATOR.to_string(), "replay_failed".to_string())]);
    assert_eq!(open(&pool, document).await?.server().seq(), 3);
    let mut transaction = begin_tenant_transaction(&pool, tenant()?).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    assert_eq!(quarantine_document(&mut transaction, document, QuarantineReason::Operator).await?, QuarantineChange::Applied);
    transaction.commit().await?;
    assert!(matches!(open(&pool, document).await, Err(RoomError::Quarantined)));
    pool.close().await;
    admin.close().await;
    Ok(())
}

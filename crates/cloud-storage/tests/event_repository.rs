use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, TenantId};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::events::{Appended, EventRepositoryError, append_events};
use figcraft_collab::{Accepted, Change, NodeId};
use sqlx::postgres::{PgPool, PgPoolOptions};

const TENANT: &str = "94000000-0000-4000-8000-000000000001";
const OTHER_TENANT: &str = "94000000-0000-4000-8000-000000000002";
const ACTOR: &str = "94000000-0000-4000-8000-000000000009";
const ACTIVE_DOCUMENT: &str = "94300000-0000-4000-8000-000000000001";
const ARCHIVED_DOCUMENT: &str = "94300000-0000-4000-8000-000000000002";

fn batch(seq: u64) -> Accepted {
    Accepted { seq, author: 1, batch_id: seq, changes: vec![Change::Delete { id: NodeId(seq) }] }
}

async fn pool() -> Result<Option<PgPool>, Box<dyn std::error::Error>> {
    let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; PostgreSQL event repository probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    Ok(Some(PgPoolOptions::new().max_connections(4).connect(database_url.trim_end()).await?))
}

#[tokio::test]
async fn append_allocates_consecutive_sequences_under_the_document_lock() -> Result<(), Box<dyn std::error::Error>> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let tenant: TenantId = TENANT.parse()?;
    let actor: PrincipalId = ACTOR.parse()?;
    let document: CloudDocumentId = ACTIVE_DOCUMENT.parse()?;

    let mut scoped = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    assert_eq!(append_events(&mut scoped, document, 1, actor, &[batch(1), batch(2)]).await?, Appended { first_seq: 1, last_seq: 2 });
    assert_eq!(append_events(&mut scoped, document, 1, actor, &[batch(3)]).await?, Appended { first_seq: 3, last_seq: 3 });
    let (current_seq,): (i64,) =
        sqlx::query_as("SELECT current_seq FROM documents WHERE id = $1::uuid").bind(ACTIVE_DOCUMENT).fetch_one(scoped.connection()).await?;
    assert_eq!(current_seq, 3);
    let (rows, max_seq): (i64, Option<i64>) =
        sqlx::query_as("SELECT count(*), max(seq) FROM document_events WHERE tenant_id = $1::uuid AND document_id = $2::uuid")
            .bind(TENANT)
            .bind(ACTIVE_DOCUMENT)
            .fetch_one(scoped.connection())
            .await?;
    assert_eq!((rows, max_seq), (3, Some(3)));

    // Gaps, repeats, rewinds and out-of-order batches are refused before anything is written, so
    // the transaction stays usable and the caller learns the committed sequence.
    let conflict = Err(EventRepositoryError::SequenceConflict { current: 3 });
    assert_eq!(append_events(&mut scoped, document, 1, actor, &[batch(5)]).await, conflict);
    assert_eq!(append_events(&mut scoped, document, 1, actor, &[batch(3)]).await, conflict);
    assert_eq!(append_events(&mut scoped, document, 1, actor, &[batch(1)]).await, conflict);
    assert_eq!(append_events(&mut scoped, document, 1, actor, &[batch(5), batch(4)]).await, conflict);
    assert_eq!(append_events(&mut scoped, document, 1, actor, &[batch(4), batch(4)]).await, conflict);
    assert_eq!(append_events(&mut scoped, document, 1, actor, &[]).await, Err(EventRepositoryError::InvalidInput));
    assert_eq!(append_events(&mut scoped, document, 1, actor, &[batch(0)]).await, Err(EventRepositoryError::InvalidInput));
    assert_eq!(append_events(&mut scoped, ARCHIVED_DOCUMENT.parse()?, 1, actor, &[batch(1)]).await, Err(EventRepositoryError::InvalidState));
    assert_eq!(append_events(&mut scoped, CloudDocumentId::random(), 1, actor, &[batch(1)]).await, Err(EventRepositoryError::NotFound));
    assert_eq!(append_events(&mut scoped, document, 1, actor, &[batch(4)]).await?, Appended { first_seq: 4, last_seq: 4 });
    scoped.rollback().await?;

    // Another tenant cannot see, and so cannot append to, this tenant's document.
    let mut other = begin_tenant_transaction(&pool, OTHER_TENANT.parse()?).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    assert_eq!(append_events(&mut other, document, 1, actor, &[batch(1)]).await, Err(EventRepositoryError::NotFound));
    other.rollback().await?;

    // A sequence or fencing token rewind is refused by the migration's trigger whatever path
    // updates the row. Each violation aborts its transaction, so each gets its own.
    for rewind in [
        "UPDATE documents SET current_seq = current_seq - 1 WHERE id = $1::uuid",
        "UPDATE documents SET fencing_token = fencing_token - 1 WHERE id = $1::uuid",
    ] {
        let mut scoped = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
        append_events(&mut scoped, document, 1, actor, &[batch(1)]).await?;
        sqlx::query("UPDATE documents SET fencing_token = 1 WHERE id = $1::uuid").bind(ACTIVE_DOCUMENT).execute(scoped.connection()).await?;
        let error = sqlx::query(rewind).bind(ACTIVE_DOCUMENT).execute(scoped.connection()).await.err().ok_or("rewind should fail")?;
        assert_eq!(error.as_database_error().and_then(|database| database.code()).as_deref(), Some("23514"));
        scoped.rollback().await?;
    }

    pool.close().await;
    Ok(())
}

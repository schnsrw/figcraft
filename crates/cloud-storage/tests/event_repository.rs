use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, TenantId};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::events::{Appended, EventRepositoryError, SequencerClaim, append_events, claim_sequencer};
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
    let claim = claim_sequencer(&mut scoped, document).await?;
    assert_eq!(claim.current_seq, 0);
    let token = claim.fencing_token;
    assert_eq!(append_events(&mut scoped, document, token, actor, &[batch(1), batch(2)]).await?, Appended { first_seq: 1, last_seq: 2 });
    assert_eq!(append_events(&mut scoped, document, token, actor, &[batch(3)]).await?, Appended { first_seq: 3, last_seq: 3 });
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
    assert_eq!(append_events(&mut scoped, document, token, actor, &[batch(5)]).await, conflict);
    assert_eq!(append_events(&mut scoped, document, token, actor, &[batch(3)]).await, conflict);
    assert_eq!(append_events(&mut scoped, document, token, actor, &[batch(1)]).await, conflict);
    assert_eq!(append_events(&mut scoped, document, token, actor, &[batch(5), batch(4)]).await, conflict);
    assert_eq!(append_events(&mut scoped, document, token, actor, &[batch(4), batch(4)]).await, conflict);
    assert_eq!(append_events(&mut scoped, document, token, actor, &[]).await, Err(EventRepositoryError::InvalidInput));
    assert_eq!(append_events(&mut scoped, document, token, actor, &[batch(0)]).await, Err(EventRepositoryError::InvalidInput));
    assert_eq!(append_events(&mut scoped, ARCHIVED_DOCUMENT.parse()?, token, actor, &[batch(1)]).await, Err(EventRepositoryError::InvalidState));
    assert_eq!(append_events(&mut scoped, CloudDocumentId::random(), token, actor, &[batch(1)]).await, Err(EventRepositoryError::NotFound));
    assert_eq!(append_events(&mut scoped, document, token, actor, &[batch(4)]).await?, Appended { first_seq: 4, last_seq: 4 });
    scoped.rollback().await?;

    // Another tenant cannot see, and so cannot append to, this tenant's document.
    let mut other = begin_tenant_transaction(&pool, OTHER_TENANT.parse()?).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    assert_eq!(append_events(&mut other, document, token, actor, &[batch(1)]).await, Err(EventRepositoryError::NotFound));
    other.rollback().await?;

    // A sequence or fencing token rewind is refused by the migration's trigger whatever path
    // updates the row. Each violation aborts its transaction, so each gets its own.
    for rewind in [
        "UPDATE documents SET current_seq = current_seq - 1 WHERE id = $1::uuid",
        "UPDATE documents SET fencing_token = fencing_token - 1 WHERE id = $1::uuid",
    ] {
        let mut scoped = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
        let token = claim_sequencer(&mut scoped, document).await?.fencing_token;
        append_events(&mut scoped, document, token, actor, &[batch(1)]).await?;
        let error = sqlx::query(rewind).bind(ACTIVE_DOCUMENT).execute(scoped.connection()).await.err().ok_or("rewind should fail")?;
        assert_eq!(error.as_database_error().and_then(|database| database.code()).as_deref(), Some("23514"));
        scoped.rollback().await?;
    }

    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn append_refuses_a_fencing_token_that_is_not_current() -> Result<(), Box<dyn std::error::Error>> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let tenant: TenantId = TENANT.parse()?;
    let actor: PrincipalId = ACTOR.parse()?;
    let document: CloudDocumentId = ACTIVE_DOCUMENT.parse()?;

    let mut scoped = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    // Token 0 is never issued, and without a claim in this transaction 1 is not current either.
    assert_eq!(append_events(&mut scoped, document, 0, actor, &[batch(1)]).await, Err(EventRepositoryError::StaleFence));
    assert_eq!(append_events(&mut scoped, document, 1, actor, &[batch(1)]).await, Err(EventRepositoryError::StaleFence));
    let first = claim_sequencer(&mut scoped, document).await?;
    append_events(&mut scoped, document, first.fencing_token, actor, &[batch(1)]).await?;
    // A takeover advances the token and resumes from the committed sequence; the old owner's
    // token, and any token not yet issued, are refused before the sequence check.
    let second = claim_sequencer(&mut scoped, document).await?;
    assert_eq!(second, SequencerClaim { fencing_token: first.fencing_token + 1, current_seq: 1 });
    assert_eq!(append_events(&mut scoped, document, first.fencing_token, actor, &[batch(2)]).await, Err(EventRepositoryError::StaleFence));
    assert_eq!(append_events(&mut scoped, document, second.fencing_token + 1, actor, &[batch(2)]).await, Err(EventRepositoryError::StaleFence));
    assert_eq!(append_events(&mut scoped, document, second.fencing_token, actor, &[batch(2)]).await?, Appended { first_seq: 2, last_seq: 2 });
    let (tokens,): (Vec<i64>,) =
        sqlx::query_as("SELECT array_agg(fencing_token ORDER BY seq) FROM document_events WHERE tenant_id = $1::uuid AND document_id = $2::uuid")
            .bind(TENANT)
            .bind(ACTIVE_DOCUMENT)
            .fetch_one(scoped.connection())
            .await?;
    assert_eq!(tokens, vec![first.fencing_token, second.fencing_token]);
    // Claims are refused on documents that cannot accept edits, without advancing their token.
    assert_eq!(claim_sequencer(&mut scoped, ARCHIVED_DOCUMENT.parse()?).await, Err(EventRepositoryError::InvalidState));
    assert_eq!(claim_sequencer(&mut scoped, CloudDocumentId::random()).await, Err(EventRepositoryError::NotFound));
    let (archived_token,): (i64,) =
        sqlx::query_as("SELECT fencing_token FROM documents WHERE id = $1::uuid").bind(ARCHIVED_DOCUMENT).fetch_one(scoped.connection()).await?;
    assert_eq!(archived_token, 0);
    scoped.rollback().await?;

    // A token committed by a takeover in another transaction fences an owner that claimed earlier.
    let mut old_owner = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let stale = claim_sequencer(&mut old_owner, document).await?;
    old_owner.commit().await?;
    let mut new_owner = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let current = claim_sequencer(&mut new_owner, document).await?;
    new_owner.commit().await?;
    let mut late_write = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let next = u64::try_from(current.current_seq + 1)?;
    assert_eq!(append_events(&mut late_write, document, stale.fencing_token, actor, &[batch(next)]).await, Err(EventRepositoryError::StaleFence));
    late_write.rollback().await?;

    pool.close().await;
    Ok(())
}

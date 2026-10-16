use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, TenantId};
use figcraft_cloud_storage::events::{
    EventRepositoryError, MAX_REPLAY_BYTES, MAX_REPLAY_EVENTS, ReplayPage, append_events, claim_sequencer, encode_event, event_checksum, read_events,
};
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use figcraft_collab::{Accepted, Change, NodeId};
use sqlx::postgres::{PgPool, PgPoolOptions};

const TENANT: &str = "95000000-0000-4000-8000-000000000001";
const OTHER_TENANT: &str = "95000000-0000-4000-8000-000000000002";
const ACTOR: &str = "95000000-0000-4000-8000-000000000009";
const CHAIN_DOCUMENT: &str = "95300000-0000-4000-8000-000000000001";
const CORRUPT_DOCUMENT: &str = "95300000-0000-4000-8000-000000000002";
const CHAIN_LENGTH: u64 = 5;

/// Batches of different sizes, so a byte budget cuts a page between particular events.
fn batch(seq: u64) -> Accepted {
    let changes = (0..=seq % 3).map(|offset| Change::Delete { id: NodeId(seq * 10 + offset) }).collect();
    Accepted { seq, author: 1, batch_id: seq, changes }
}

async fn pool() -> Result<Option<PgPool>, Box<dyn std::error::Error>> {
    let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; PostgreSQL event replay probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    Ok(Some(PgPoolOptions::new().max_connections(4).connect(database_url.trim_end()).await?))
}

async fn begin<'a>(pool: &'a PgPool, tenant: &str) -> Result<TenantTransaction<'a>, Box<dyn std::error::Error>> {
    Ok(begin_tenant_transaction(pool, tenant.parse()?).await.map_err(|error| format!("begin tenant: {error:?}"))?)
}

/// Replay every page from `after_seq` until caught up, checking each page's bounds.
async fn replay_all(
    scoped: &mut TenantTransaction<'_>,
    document: CloudDocumentId,
    after_seq: i64,
    max_events: usize,
    max_bytes: usize,
) -> Result<Vec<ReplayPage>, Box<dyn std::error::Error>> {
    let mut pages = Vec::new();
    let mut after = after_seq;
    loop {
        let page = read_events(scoped, document, after, max_events, max_bytes).await?;
        assert!(page.events.len() <= max_events.min(MAX_REPLAY_EVENTS));
        assert!(page.next_after_seq() > after || !page.has_more(), "a page must progress");
        after = page.next_after_seq();
        let more = page.has_more();
        pages.push(page);
        if !more {
            return Ok(pages);
        }
    }
}

#[tokio::test]
async fn replay_returns_committed_events_in_bounded_contiguous_pages() -> Result<(), Box<dyn std::error::Error>> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let tenant: TenantId = TENANT.parse()?;
    let actor: PrincipalId = ACTOR.parse()?;
    let document: CloudDocumentId = CHAIN_DOCUMENT.parse()?;

    // Commit a fresh chain after whatever earlier runs on this fixture left behind.
    let mut writer = begin(&pool, TENANT).await?;
    let claim = claim_sequencer(&mut writer, document).await?;
    let start = claim.current_seq;
    let first = u64::try_from(start)? + 1;
    let batches: Vec<Accepted> = (first..first + CHAIN_LENGTH).map(batch).collect();
    append_events(&mut writer, document, claim.fencing_token, actor, &batches).await?;
    writer.commit().await?;
    let current = start + i64::try_from(CHAIN_LENGTH)?;

    let mut scoped = begin(&pool, TENANT).await?;
    // The new chain in one page, with its row metadata.
    let page = read_events(&mut scoped, document, start, MAX_REPLAY_EVENTS, MAX_REPLAY_BYTES).await?;
    assert_eq!(page.current_seq, current);
    assert!(!page.has_more());
    assert_eq!(page.events.iter().map(|event| event.batch.clone()).collect::<Vec<_>>(), batches);
    assert_eq!(page.events.iter().map(|event| event.seq).collect::<Vec<_>>(), (start + 1..=current).collect::<Vec<_>>());
    assert!(page.events.iter().all(|event| event.fencing_token == claim.fencing_token && event.actor == actor));

    // From zero: all history, contiguous from 1, ending with this run's chain.
    let pages = replay_all(&mut scoped, document, 0, usize::MAX, usize::MAX).await?;
    let history: Vec<_> = pages.into_iter().flat_map(|page| page.events).collect();
    assert_eq!(history.iter().map(|event| event.seq).collect::<Vec<_>>(), (1..=current).collect::<Vec<_>>());
    assert!(history.iter().all(|event| i64::try_from(event.batch.seq).ok() == Some(event.seq)));
    assert_eq!(
        history.get(usize::try_from(start)?..).map(|tail| tail.iter().map(|event| event.batch.clone()).collect::<Vec<_>>()),
        Some(batches.clone())
    );

    // From the middle, and from the committed sequence (empty and caught up).
    let middle = read_events(&mut scoped, document, start + 2, MAX_REPLAY_EVENTS, MAX_REPLAY_BYTES).await?;
    assert_eq!(middle.events.iter().map(|event| event.seq).collect::<Vec<_>>(), (start + 3..=current).collect::<Vec<_>>());
    let caught_up = read_events(&mut scoped, document, current, MAX_REPLAY_EVENTS, MAX_REPLAY_BYTES).await?;
    assert!(caught_up.events.is_empty());
    assert_eq!((caught_up.next_after_seq(), caught_up.has_more()), (current, false));

    // A count cap of two pages the chain as 2, 2, 1.
    let pages = replay_all(&mut scoped, document, start, 2, MAX_REPLAY_BYTES).await?;
    assert_eq!(pages.iter().map(|page| page.events.len()).collect::<Vec<_>>(), vec![2, 2, 1]);
    assert_eq!(pages.iter().flat_map(|page| page.events.iter().map(|event| event.batch.clone())).collect::<Vec<_>>(), batches);

    // A one-byte budget still returns one event per page, so replay progresses.
    let pages = replay_all(&mut scoped, document, start, MAX_REPLAY_EVENTS, 1).await?;
    assert_eq!(pages.iter().map(|page| page.events.len()).collect::<Vec<_>>(), vec![1; 5]);
    let pages = replay_all(&mut scoped, document, start, MAX_REPLAY_EVENTS, 0).await?;
    assert_eq!(pages.len(), 5);

    // A budget of exactly the first two payloads cuts the page after them, one byte less after one.
    let sizes: Vec<usize> =
        batches.iter().map(|batch| encode_event(tenant, document, batch).map(|event| event.payload.len())).collect::<Result<_, _>>()?;
    let two = sizes.iter().take(2).sum::<usize>();
    assert_eq!(read_events(&mut scoped, document, start, MAX_REPLAY_EVENTS, two).await?.events.len(), 2);
    assert_eq!(read_events(&mut scoped, document, start, MAX_REPLAY_EVENTS, two - 1).await?.events.len(), 1);

    // Unusable requests and documents this tenant cannot see.
    assert_eq!(read_events(&mut scoped, document, -1, 10, 10).await, Err(EventRepositoryError::InvalidInput));
    assert_eq!(read_events(&mut scoped, document, start, 0, 10).await, Err(EventRepositoryError::InvalidInput));
    assert_eq!(read_events(&mut scoped, document, current + 1, 10, 10).await, Err(EventRepositoryError::SequenceConflict { current }));
    assert_eq!(read_events(&mut scoped, CloudDocumentId::random(), 0, 10, 10).await, Err(EventRepositoryError::NotFound));
    scoped.rollback().await?;

    let mut other = begin(&pool, OTHER_TENANT).await?;
    assert_eq!(read_events(&mut other, document, 0, 10, 10).await, Err(EventRepositoryError::NotFound));
    other.rollback().await?;

    pool.close().await;
    Ok(())
}

/// Write a raw event row and advance the document's committed sequence, bypassing
/// `append_events`, as a damaged or tampered log would look.
async fn plant(scoped: &mut TenantTransaction<'_>, seq: i64, payload: &[u8], checksum: &[u8], current_seq: i64) -> Result<(), sqlx::Error> {
    if !payload.is_empty() {
        sqlx::query(
            "INSERT INTO document_events (tenant_id, document_id, seq, fencing_token, payload, checksum, actor_principal_id) \
             VALUES ($1::uuid, $2::uuid, $3, 1, $4, $5, $6::uuid)",
        )
        .bind(TENANT)
        .bind(CORRUPT_DOCUMENT)
        .bind(seq)
        .bind(payload)
        .bind(checksum)
        .bind(ACTOR)
        .execute(scoped.connection())
        .await?;
    }
    sqlx::query("UPDATE documents SET current_seq = $2 WHERE id = $1::uuid")
        .bind(CORRUPT_DOCUMENT)
        .bind(current_seq)
        .execute(scoped.connection())
        .await?;
    Ok(())
}

#[tokio::test]
async fn replay_refuses_corrupt_or_incomplete_history() -> Result<(), Box<dyn std::error::Error>> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let tenant: TenantId = TENANT.parse()?;
    let document: CloudDocumentId = CORRUPT_DOCUMENT.parse()?;
    let other_document: CloudDocumentId = CHAIN_DOCUMENT.parse()?;
    let good = |seq: u64| encode_event(tenant, document, &batch(seq));
    let first = good(1)?;
    let second = good(2)?;
    let relocated = encode_event(tenant, other_document, &batch(1))?;
    let garbage = [0xff_u8; 8];

    // The runtime role may insert events but never update or delete them, so each damaged log is
    // planted inside a transaction that is rolled back; the document stays empty across reruns.
    type Plant<'a> = &'a [(i64, &'a [u8], &'a [u8])];
    let cases: [(&str, Plant<'_>, i64); 6] = [
        ("wrong checksum", &[(1, &first.payload, &[0; 32])], 1),
        ("checksummed garbage", &[(1, &garbage, &event_checksum(tenant, document, 1, &garbage))], 1),
        ("payload relocated from another document", &[(1, &relocated.payload, &relocated.checksum)], 1),
        ("gap before the first event", &[(2, &second.payload, &second.checksum)], 2),
        ("committed sequence with no events", &[], 1),
        ("corrupt event after a good one", &[(1, &first.payload, &first.checksum), (2, &second.payload, &[1; 32])], 2),
    ];
    for (name, rows, current_seq) in cases {
        let mut scoped = begin(&pool, TENANT).await?;
        for (seq, payload, checksum) in rows {
            plant(&mut scoped, *seq, payload, checksum, *seq).await?;
        }
        plant(&mut scoped, current_seq, &[], &[], current_seq).await?;
        assert_eq!(read_events(&mut scoped, document, 0, MAX_REPLAY_EVENTS, MAX_REPLAY_BYTES).await, Err(EventRepositoryError::Corrupt), "{name}");
        scoped.rollback().await?;
    }

    // A page that stops before the damage still replays, so a caller sees exactly where it is.
    let mut scoped = begin(&pool, TENANT).await?;
    plant(&mut scoped, 1, &first.payload, &first.checksum, 1).await?;
    plant(&mut scoped, 2, &second.payload, &[1; 32], 2).await?;
    let page = read_events(&mut scoped, document, 0, 1, MAX_REPLAY_BYTES).await?;
    assert_eq!((page.next_after_seq(), page.has_more()), (1, true));
    assert_eq!(page.events.first().map(|event| event.batch.clone()), Some(batch(1)));
    assert_eq!(read_events(&mut scoped, document, 1, 1, MAX_REPLAY_BYTES).await, Err(EventRepositoryError::Corrupt));
    scoped.rollback().await?;

    pool.close().await;
    Ok(())
}

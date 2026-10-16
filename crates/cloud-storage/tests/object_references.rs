//! S3-007: object-reference accounting, retention holds, collection candidates and the race
//! between adding a reference and starting an object's deletion, against PostgreSQL.
//!
//! Objects are registered under unique per-run keys and backdated with plain SQL where an age
//! matters. Racers are separate connections on a multi-thread runtime; where an ordering matters,
//! the test waits through `pg_stat_activity` until the second transaction is blocked on the first's
//! row lock before releasing it, so no sleep stands in for synchronisation.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use figcraft_cloud_domain::{TenantId, Uuid};
use figcraft_cloud_storage::object_metadata::{NewObject, ObjectKind, ObjectState, get_object, keys, register_object};
use figcraft_cloud_storage::object_references::{
    Change, DeletionStart, MAX_COLLECTABLE_PAGE, MAX_HOLDS_PER_OBJECT, MAX_REFERENCE_PAGE, MIN_COLLECTION_AGE, ObjectReferenceError, OwnerKind,
    ReferenceCount, ReferenceOwner, add_reference, begin_object_deletion, collectable_objects, list_references, place_retention_hold,
    reference_count, release_retention_hold, remove_reference,
};
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgPool, PgPoolOptions};
use tokio::sync::Barrier;
use tokio::time::timeout;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type TestResult<T = ()> = Result<T, BoxError>;

const TENANT: &str = "a5000000-0000-4000-8000-000000000001";
const OTHER_TENANT: &str = "a5000000-0000-4000-8000-000000000002";
const BUCKET: &str = "figcraft";
/// Concurrent reference/deletion pairs in the free-running race.
const RACES: usize = 8;
/// Upper bound on any single wait, so a lost wake-up fails the test instead of hanging it.
const BOUND: Duration = Duration::from_secs(30);

async fn pool() -> TestResult<Option<PgPool>> {
    let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; object reference probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    let connections = u32::try_from(RACES * 2 + 2)?;
    Ok(Some(PgPoolOptions::new().max_connections(connections).acquire_timeout(BOUND).connect(database_url.trim_end()).await?))
}

async fn begin_as<'a>(pool: &'a PgPool, tenant: &str) -> TestResult<TenantTransaction<'a>> {
    let tenant: TenantId = tenant.parse()?;
    let mut transaction = begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    sqlx::query("SET LOCAL lock_timeout = '20s'").execute(transaction.connection()).await?;
    Ok(transaction)
}

async fn begin(pool: &PgPool) -> TestResult<TenantTransaction<'_>> {
    begin_as(pool, TENANT).await
}

/// Register an object under a unique key, in `state`, created `age` ago.
async fn object(transaction: &mut TenantTransaction<'_>, state: &str, age: Duration) -> TestResult<Uuid> {
    let sha256: [u8; 32] = Sha256::digest(format!("figcraft reference probe {}", Uuid::new_v4())).into();
    let object = NewObject {
        id: Uuid::new_v4(),
        kind: ObjectKind::Asset,
        object_key: keys::asset(transaction.tenant_id(), &sha256),
        sha256,
        size_bytes: 1,
        media_type: "application/octet-stream".into(),
        producer_version: "figcraft-cloud test".into(),
    };
    register_object(transaction, BUCKET, &object).await?;
    sqlx::query(concat!(
        "UPDATE objects SET state = $3, created_at = now() - $4::bigint * interval '1 second', ",
        "verified_at = CASE WHEN $3 = 'verified' THEN now() END, ",
        "deleted_at = CASE WHEN $3 IN ('deleting', 'deleted') THEN now() END ",
        "WHERE tenant_id = $1::uuid AND id = $2::uuid"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(object.id.to_string())
    .bind(state)
    .bind(i64::try_from(age.as_secs())?)
    .execute(transaction.connection())
    .await?;
    Ok(object.id)
}

async fn state(pool: &PgPool, id: Uuid) -> TestResult<Option<ObjectState>> {
    let mut transaction = begin(pool).await?;
    let state = get_object(&mut transaction, id).await?.map(|object| object.state);
    transaction.rollback().await?;
    Ok(state)
}

async fn count(pool: &PgPool, id: Uuid) -> TestResult<ReferenceCount> {
    let mut transaction = begin(pool).await?;
    let count = reference_count(&mut transaction, id).await?;
    transaction.rollback().await?;
    Ok(count)
}

async fn backend_pid(transaction: &mut TenantTransaction<'_>) -> TestResult<i32> {
    Ok(sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(transaction.connection()).await?)
}

/// Wait until `pid` is blocked on a heavyweight lock.
async fn wait_until_blocked(pool: &PgPool, pid: i32) -> TestResult {
    let deadline = Instant::now() + BOUND;
    loop {
        let waiting: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1 AND wait_event_type = 'Lock')")
            .bind(pid)
            .fetch_one(pool)
            .await?;
        if waiting {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err("racer never blocked on the object row lock".into());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

const OLD: Duration = Duration::from_secs(2 * 24 * 60 * 60);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn references_and_holds_are_idempotent_and_state_checked() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let mut scoped = begin(&pool).await?;
    let document = Uuid::new_v4();
    let snapshot = Uuid::new_v4();
    let hold = Uuid::new_v4();

    // Pending and verified objects gain references; adding twice is a no-op.
    let pending = object(&mut scoped, "pending", Duration::ZERO).await?;
    let verified = object(&mut scoped, "verified", Duration::ZERO).await?;
    assert_eq!(add_reference(&mut scoped, pending, OwnerKind::Document, document).await?, Change::Applied);
    assert_eq!(add_reference(&mut scoped, pending, OwnerKind::Document, document).await?, Change::AlreadyApplied);
    assert_eq!(add_reference(&mut scoped, verified, OwnerKind::Document, document).await?, Change::Applied);
    assert_eq!(add_reference(&mut scoped, verified, OwnerKind::DocumentSnapshot, snapshot).await?, Change::Applied);
    assert_eq!(place_retention_hold(&mut scoped, verified, hold).await?, Change::Applied);
    assert_eq!(place_retention_hold(&mut scoped, verified, hold).await?, Change::AlreadyApplied);
    assert_eq!(reference_count(&mut scoped, verified).await?, ReferenceCount { references: 2, holds: 1 });
    let listed: HashSet<ReferenceOwner> = list_references(&mut scoped, verified, MAX_REFERENCE_PAGE).await?.into_iter().collect();
    let expected: HashSet<ReferenceOwner> = [
        ReferenceOwner::Owner(OwnerKind::Document, document),
        ReferenceOwner::Owner(OwnerKind::DocumentSnapshot, snapshot),
        ReferenceOwner::RetentionHold(hold),
    ]
    .into_iter()
    .collect();
    assert_eq!(listed, expected);
    assert_eq!(list_references(&mut scoped, verified, 1).await?.len(), 1);
    assert_eq!(list_references(&mut scoped, verified, 0).await, Err(ObjectReferenceError::InvalidLimit));
    assert_eq!(list_references(&mut scoped, verified, MAX_REFERENCE_PAGE + 1).await, Err(ObjectReferenceError::InvalidLimit));

    // A hold is not an ordinary reference: removing a document reference leaves it, and only
    // releasing it removes it. Removal and release are idempotent.
    assert_eq!(remove_reference(&mut scoped, verified, OwnerKind::Document, document).await?, Change::Applied);
    assert_eq!(remove_reference(&mut scoped, verified, OwnerKind::Document, document).await?, Change::AlreadyApplied);
    assert_eq!(remove_reference(&mut scoped, verified, OwnerKind::Document, hold).await?, Change::AlreadyApplied);
    assert_eq!(remove_reference(&mut scoped, verified, OwnerKind::DocumentSnapshot, snapshot).await?, Change::Applied);
    assert_eq!(reference_count(&mut scoped, verified).await?, ReferenceCount { references: 0, holds: 1 });
    assert_eq!(release_retention_hold(&mut scoped, verified, hold).await?, Change::Applied);
    assert_eq!(release_retention_hold(&mut scoped, verified, hold).await?, Change::AlreadyApplied);
    assert!(reference_count(&mut scoped, verified).await?.is_unreferenced());

    // Rejected objects take holds but no references; deleting and deleted ones take neither, and
    // unknown objects are NotFound. An existing reference stays AlreadyApplied after rejection.
    let rejected = object(&mut scoped, "rejected", Duration::ZERO).await?;
    let deleting = object(&mut scoped, "deleting", Duration::ZERO).await?;
    let deleted = object(&mut scoped, "deleted", Duration::ZERO).await?;
    assert_eq!(add_reference(&mut scoped, rejected, OwnerKind::Asset, Uuid::new_v4()).await, Err(ObjectReferenceError::InvalidState));
    assert_eq!(place_retention_hold(&mut scoped, rejected, hold).await?, Change::Applied);
    for gone in [deleting, deleted] {
        assert_eq!(add_reference(&mut scoped, gone, OwnerKind::Asset, Uuid::new_v4()).await, Err(ObjectReferenceError::InvalidState));
        assert_eq!(place_retention_hold(&mut scoped, gone, hold).await, Err(ObjectReferenceError::InvalidState));
    }
    assert_eq!(add_reference(&mut scoped, Uuid::new_v4(), OwnerKind::Asset, document).await, Err(ObjectReferenceError::NotFound));
    assert_eq!(place_retention_hold(&mut scoped, Uuid::new_v4(), hold).await, Err(ObjectReferenceError::NotFound));
    sqlx::query("UPDATE objects SET state = 'rejected' WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(TENANT)
        .bind(pending.to_string())
        .execute(scoped.connection())
        .await?;
    assert_eq!(add_reference(&mut scoped, pending, OwnerKind::Document, document).await?, Change::AlreadyApplied);
    assert_eq!(add_reference(&mut scoped, pending, OwnerKind::Export, document).await, Err(ObjectReferenceError::InvalidState));

    // Holds per object are capped.
    for _ in 1..MAX_HOLDS_PER_OBJECT {
        place_retention_hold(&mut scoped, rejected, Uuid::new_v4()).await?;
    }
    assert_eq!(reference_count(&mut scoped, rejected).await?.holds, MAX_HOLDS_PER_OBJECT);
    assert_eq!(place_retention_hold(&mut scoped, rejected, Uuid::new_v4()).await, Err(ObjectReferenceError::TooManyHolds));
    assert_eq!(place_retention_hold(&mut scoped, rejected, hold).await?, Change::AlreadyApplied);
    scoped.rollback().await?;

    // Another tenant cannot see, reference, hold or delete this tenant's object.
    let mut owner = begin(&pool).await?;
    let committed = object(&mut owner, "verified", OLD).await?;
    owner.commit().await?;
    let mut other = begin_as(&pool, OTHER_TENANT).await?;
    assert_eq!(add_reference(&mut other, committed, OwnerKind::Document, document).await, Err(ObjectReferenceError::NotFound));
    assert_eq!(place_retention_hold(&mut other, committed, hold).await, Err(ObjectReferenceError::NotFound));
    assert_eq!(begin_object_deletion(&mut other, committed).await, Err(ObjectReferenceError::NotFound));
    assert_eq!(remove_reference(&mut other, committed, OwnerKind::Document, document).await?, Change::AlreadyApplied);
    assert!(collectable_objects(&mut other, MIN_COLLECTION_AGE, MAX_COLLECTABLE_PAGE).await?.iter().all(|candidate| candidate.id != committed));
    other.rollback().await?;
    assert_eq!(state(&pool, committed).await?, Some(ObjectState::Verified));

    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_old_unreferenced_unheld_objects_are_collectable() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let hour = Duration::from_secs(60 * 60);
    let mut scoped = begin(&pool).await?;
    let old_verified = object(&mut scoped, "verified", OLD).await?;
    let old_pending = object(&mut scoped, "pending", OLD + hour).await?;
    let old_rejected = object(&mut scoped, "rejected", OLD + hour * 2).await?;
    let ancient = object(&mut scoped, "verified", OLD * 3).await?;
    let referenced = object(&mut scoped, "verified", OLD).await?;
    add_reference(&mut scoped, referenced, OwnerKind::Thumbnail, Uuid::new_v4()).await?;
    let held = object(&mut scoped, "rejected", OLD).await?;
    place_retention_hold(&mut scoped, held, Uuid::new_v4()).await?;
    let fresh = object(&mut scoped, "verified", Duration::ZERO).await?;
    let nearly = object(&mut scoped, "verified", MIN_COLLECTION_AGE - hour).await?;
    let old_deleting = object(&mut scoped, "deleting", OLD).await?;
    let old_deleted = object(&mut scoped, "deleted", OLD).await?;
    let mine = [old_verified, old_pending, old_rejected, ancient, referenced, held, fresh, nearly, old_deleting, old_deleted];

    let candidates = |found: Vec<_>| -> HashSet<Uuid> {
        found
            .into_iter()
            .map(|candidate: figcraft_cloud_storage::object_references::CollectableObject| candidate.id)
            .filter(|id| mine.contains(id))
            .collect()
    };
    let expected: HashSet<Uuid> = [old_verified, old_pending, old_rejected, ancient].into_iter().collect();
    // A zero (or any sub-floor) minimum age is raised to the floor, so `nearly` stays protected.
    assert_eq!(candidates(collectable_objects(&mut scoped, Duration::ZERO, MAX_COLLECTABLE_PAGE).await?), expected);
    assert_eq!(candidates(collectable_objects(&mut scoped, MIN_COLLECTION_AGE, MAX_COLLECTABLE_PAGE).await?), expected);
    // A longer minimum age narrows the list; an absurd one is capped rather than overflowing.
    assert_eq!(candidates(collectable_objects(&mut scoped, OLD * 2, MAX_COLLECTABLE_PAGE).await?), [ancient].into_iter().collect());
    assert!(candidates(collectable_objects(&mut scoped, Duration::MAX, MAX_COLLECTABLE_PAGE).await?).is_empty());
    // Oldest first, and the page is bounded.
    let first = collectable_objects(&mut scoped, Duration::ZERO, 1).await?;
    assert_eq!(first.len(), 1);
    assert_eq!(collectable_objects(&mut scoped, Duration::ZERO, 0).await, Err(ObjectReferenceError::InvalidLimit));
    assert_eq!(collectable_objects(&mut scoped, Duration::ZERO, MAX_COLLECTABLE_PAGE + 1).await, Err(ObjectReferenceError::InvalidLimit));
    let candidate = collectable_objects(&mut scoped, Duration::ZERO, MAX_COLLECTABLE_PAGE).await?;
    assert!(candidate.iter().position(|c| c.id == ancient) < candidate.iter().position(|c| c.id == old_verified));

    // Deletion re-checks everything under the lock.
    let Ok(DeletionStart::Marked(marked)) = begin_object_deletion(&mut scoped, old_verified).await else {
        return Err("an old unreferenced object should be marked".into());
    };
    assert_eq!(marked.bucket, BUCKET);
    assert!(marked.object_key.starts_with(&format!("tenants/{TENANT}/assets/")));
    assert!(matches!(begin_object_deletion(&mut scoped, old_verified).await, Ok(DeletionStart::AlreadyDeleting(_))));
    assert!(matches!(begin_object_deletion(&mut scoped, old_pending).await, Ok(DeletionStart::Marked(_))));
    assert!(matches!(begin_object_deletion(&mut scoped, old_rejected).await, Ok(DeletionStart::Marked(_))));
    assert!(matches!(begin_object_deletion(&mut scoped, old_deleting).await, Ok(DeletionStart::AlreadyDeleting(_))));
    assert_eq!(begin_object_deletion(&mut scoped, referenced).await, Err(ObjectReferenceError::Referenced));
    assert_eq!(begin_object_deletion(&mut scoped, held).await, Err(ObjectReferenceError::Referenced));
    assert_eq!(begin_object_deletion(&mut scoped, fresh).await, Err(ObjectReferenceError::TooYoung));
    assert_eq!(begin_object_deletion(&mut scoped, nearly).await, Err(ObjectReferenceError::TooYoung));
    assert_eq!(begin_object_deletion(&mut scoped, old_deleted).await, Err(ObjectReferenceError::InvalidState));
    assert_eq!(begin_object_deletion(&mut scoped, Uuid::new_v4()).await, Err(ObjectReferenceError::NotFound));
    let after = get_object(&mut scoped, old_verified).await?.map(|object| object.state);
    assert_eq!(after, Some(ObjectState::Deleting));
    let deleted_at_set: bool = sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM objects WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(TENANT)
        .bind(old_verified.to_string())
        .fetch_one(scoped.connection())
        .await?;
    assert!(deleted_at_set);
    // Marked objects leave the candidate list and refuse new references; releasing the last hold
    // makes an object collectable again.
    assert_eq!(candidates(collectable_objects(&mut scoped, Duration::ZERO, MAX_COLLECTABLE_PAGE).await?), [ancient].into_iter().collect());
    assert_eq!(add_reference(&mut scoped, old_verified, OwnerKind::Document, Uuid::new_v4()).await, Err(ObjectReferenceError::InvalidState));
    let holds = list_references(&mut scoped, held, MAX_REFERENCE_PAGE).await?;
    for owner in holds {
        if let ReferenceOwner::RetentionHold(hold) = owner {
            release_retention_hold(&mut scoped, held, hold).await?;
        }
    }
    assert!(candidates(collectable_objects(&mut scoped, Duration::ZERO, MAX_COLLECTABLE_PAGE).await?).contains(&held));
    scoped.rollback().await?;
    pool.close().await;
    Ok(())
}

async fn committed_object(pool: &PgPool) -> TestResult<Uuid> {
    let mut transaction = begin(pool).await?;
    let id = object(&mut transaction, "verified", OLD).await?;
    transaction.commit().await?;
    Ok(id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reference_committed_first_blocks_deletion() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    for hold in [false, true] {
        let id = committed_object(&pool).await?;
        let mut first = begin(&pool).await?;
        if hold {
            assert_eq!(place_retention_hold(&mut first, id, Uuid::new_v4()).await?, Change::Applied);
        } else {
            assert_eq!(add_reference(&mut first, id, OwnerKind::Document, Uuid::new_v4()).await?, Change::Applied);
        }
        let (pid_sender, pid_receiver) = tokio::sync::oneshot::channel();
        let racer = {
            let pool = pool.clone();
            tokio::spawn(async move {
                let mut transaction = begin(&pool).await?;
                pid_sender.send(backend_pid(&mut transaction).await?).map_err(|_| "pid receiver dropped")?;
                let result = begin_object_deletion(&mut transaction, id).await;
                transaction.commit().await?;
                TestResult::Ok(result)
            })
        };
        wait_until_blocked(&pool, timeout(BOUND, pid_receiver).await??).await?;
        first.commit().await?;
        assert_eq!(timeout(BOUND, racer).await???, Err(ObjectReferenceError::Referenced));
        assert_eq!(state(&pool, id).await?, Some(ObjectState::Verified));
        let counted = count(&pool, id).await?;
        assert_eq!(counted.references + counted.holds, 1);
    }
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deletion_marked_first_refuses_the_waiting_reference() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    for hold in [false, true] {
        let id = committed_object(&pool).await?;
        let mut first = begin(&pool).await?;
        assert!(matches!(begin_object_deletion(&mut first, id).await?, DeletionStart::Marked(_)));
        let (pid_sender, pid_receiver) = tokio::sync::oneshot::channel();
        let racer = {
            let pool = pool.clone();
            tokio::spawn(async move {
                let mut transaction = begin(&pool).await?;
                pid_sender.send(backend_pid(&mut transaction).await?).map_err(|_| "pid receiver dropped")?;
                let result = if hold {
                    place_retention_hold(&mut transaction, id, Uuid::new_v4()).await
                } else {
                    add_reference(&mut transaction, id, OwnerKind::Document, Uuid::new_v4()).await
                };
                transaction.commit().await?;
                TestResult::Ok(result)
            })
        };
        wait_until_blocked(&pool, timeout(BOUND, pid_receiver).await??).await?;
        first.commit().await?;
        assert_eq!(timeout(BOUND, racer).await???, Err(ObjectReferenceError::InvalidState));
        assert_eq!(state(&pool, id).await?, Some(ObjectState::Deleting));
        assert!(count(&pool, id).await?.is_unreferenced());
    }

    // A rolled-back deletion releases the lock without effect, and the waiting reference lands.
    let id = committed_object(&pool).await?;
    let mut first = begin(&pool).await?;
    assert!(matches!(begin_object_deletion(&mut first, id).await?, DeletionStart::Marked(_)));
    let (pid_sender, pid_receiver) = tokio::sync::oneshot::channel();
    let racer = {
        let pool = pool.clone();
        tokio::spawn(async move {
            let mut transaction = begin(&pool).await?;
            pid_sender.send(backend_pid(&mut transaction).await?).map_err(|_| "pid receiver dropped")?;
            let result = add_reference(&mut transaction, id, OwnerKind::Document, Uuid::new_v4()).await;
            transaction.commit().await?;
            TestResult::Ok(result)
        })
    };
    wait_until_blocked(&pool, timeout(BOUND, pid_receiver).await??).await?;
    first.rollback().await?;
    assert_eq!(timeout(BOUND, racer).await???, Ok(Change::Applied));
    assert_eq!(state(&pool, id).await?, Some(ObjectState::Verified));
    pool.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn free_running_races_never_leave_a_reference_on_a_deleting_object() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let mut ids = Vec::with_capacity(RACES);
    for _ in 0..RACES {
        ids.push(committed_object(&pool).await?);
    }
    let barrier = std::sync::Arc::new(Barrier::new(RACES * 2));
    let mut referencers = Vec::with_capacity(RACES);
    let mut deleters = Vec::with_capacity(RACES);
    for &id in &ids {
        let (pool_a, barrier_a) = (pool.clone(), barrier.clone());
        referencers.push(tokio::spawn(async move {
            let mut transaction = begin(&pool_a).await?;
            barrier_a.wait().await;
            let result = add_reference(&mut transaction, id, OwnerKind::Document, Uuid::new_v4()).await;
            transaction.commit().await?;
            TestResult::Ok(result)
        }));
        let (pool_b, barrier_b) = (pool.clone(), barrier.clone());
        deleters.push(tokio::spawn(async move {
            let mut transaction = begin(&pool_b).await?;
            barrier_b.wait().await;
            let result = begin_object_deletion(&mut transaction, id).await;
            transaction.commit().await?;
            TestResult::Ok(result)
        }));
    }
    for ((id, referencer), deleter) in ids.into_iter().zip(referencers).zip(deleters) {
        let reference = timeout(BOUND, referencer).await???;
        let deletion = timeout(BOUND, deleter).await???;
        let final_state = state(&pool, id).await?;
        let counted = count(&pool, id).await?;
        match (reference, deletion) {
            (Ok(Change::Applied), Err(ObjectReferenceError::Referenced)) => {
                assert_eq!(final_state, Some(ObjectState::Verified));
                assert_eq!(counted.references, 1);
            }
            (Err(ObjectReferenceError::InvalidState), Ok(DeletionStart::Marked(_))) => {
                assert_eq!(final_state, Some(ObjectState::Deleting));
                assert!(counted.is_unreferenced());
            }
            other => return Err(format!("exactly one racer must win, got {other:?}").into()),
        }
    }
    pool.close().await;
    Ok(())
}

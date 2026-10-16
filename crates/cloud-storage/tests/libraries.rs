//! S3-010: team-library creation, versioned publishing, team grants, per-document usage and
//! archiving, against PostgreSQL and the S3-compatible store.
//!
//! Every published archive is a synthetic `.figcraft` zip unique to the run, so content-addressed
//! keys never collide across reruns; the test deletes the objects it wrote. Racing publishers are
//! separate connections on a multi-thread runtime; the ordered race waits through
//! `pg_stat_activity` until the second publisher is blocked on the library row lock.

// Fixture identifiers are literals and the zip writer targets memory, so those unwraps cannot fail.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::io::{Cursor, Write};
use std::time::{Duration, Instant};

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, TenantId, Uuid};
use figcraft_cloud_storage::libraries::{
    Change, LibraryError, LibraryRole, LibraryState, LibraryUsage, LibraryVersion, MAX_LIBRARY_PAGE, PublishedVersion, UsageChange, archive_library,
    create_library, get_library, get_version, grant_library_to_team, list_grants, list_versions, publish_library_version, record_library_usage,
    remove_library_usage, revoke_library_grant, usage_of_library, usages_of_version,
};
use figcraft_cloud_storage::object_metadata::{ObjectKind, ObjectState, keys};
use figcraft_cloud_storage::object_references::{OwnerKind, ReferenceCount, ReferenceOwner, list_references, reference_count};
use figcraft_cloud_storage::objects::{ObjectStore, ObjectStoreSettings};
use figcraft_cloud_storage::validation::Rejection;
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgPool, PgPoolOptions};
use tokio::sync::Barrier;
use tokio::time::timeout;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type TestResult<T = ()> = Result<T, BoxError>;

const TENANT: &str = "a8000000-0000-4000-8000-000000000001";
const OTHER_TENANT: &str = "a8000000-0000-4000-8000-000000000002";
/// Owning-team editor.
const TEAM_EDITOR: &str = "a8000000-0000-4000-8000-000000000009";
/// Owning-team viewer.
const TEAM_VIEWER: &str = "a8000000-0000-4000-8000-00000000000a";
/// Member of the other tenant only.
const OUTSIDER: &str = "a8000000-0000-4000-8000-00000000000b";
/// Editor of the team the tests grant `publisher`.
const GRANTED_EDITOR: &str = "a8000000-0000-4000-8000-00000000000c";
const TENANT_ADMIN: &str = "a8000000-0000-4000-8000-00000000000d";
const TENANT_EDITOR: &str = "a8000000-0000-4000-8000-00000000000e";
const OWNING_TEAM: &str = "a8000000-0000-4000-8000-000000000011";
const GRANTED_TEAM: &str = "a8000000-0000-4000-8000-000000000012";
const ARCHIVED_TEAM: &str = "a8000000-0000-4000-8000-000000000013";
const OTHER_TEAM: &str = "a8000000-0000-4000-8000-000000000021";
const SOURCE_DOCUMENT: &str = "a8000000-0000-4000-8000-000000000051";
const CONSUMER: &str = "a8000000-0000-4000-8000-000000000052";
const ARCHIVED_DOCUMENT: &str = "a8000000-0000-4000-8000-000000000053";
const DELETING_DOCUMENT: &str = "a8000000-0000-4000-8000-000000000054";
const OTHER_DOCUMENT: &str = "a8000000-0000-4000-8000-000000000061";
/// Free-running concurrent publishers.
const RACERS: usize = 4;
/// Upper bound on any single wait, so a lost wake-up fails the test instead of hanging it.
const BOUND: Duration = Duration::from_secs(60);

async fn probe() -> TestResult<Option<(PgPool, ObjectStore)>> {
    let (Ok(database_url_file), Ok(s3_directory)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR")) else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; library probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    let connections = u32::try_from(RACERS + 4)?;
    let pool = PgPoolOptions::new().max_connections(connections).acquire_timeout(BOUND).connect(database_url.trim_end()).await?;
    let read =
        |name: &str| -> TestResult<String> { Ok(std::fs::read_to_string(std::path::Path::new(&s3_directory).join(name))?.trim_end().to_string()) };
    let settings = ObjectStoreSettings {
        endpoint: read("s3_endpoint")?,
        region: "us-east-1".into(),
        bucket: "figcraft".into(),
        access_key: read("s3_access_key")?,
        secret_key: read("s3_secret_key")?,
        path_style: true,
    };
    Ok(Some((pool, ObjectStore::new(&settings)?)))
}

async fn begin_as<'a>(pool: &'a PgPool, tenant: &str) -> TestResult<TenantTransaction<'a>> {
    let tenant: TenantId = tenant.parse()?;
    let mut transaction = begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    sqlx::query("SET LOCAL lock_timeout = '30s'").execute(transaction.connection()).await?;
    Ok(transaction)
}

async fn begin(pool: &PgPool) -> TestResult<TenantTransaction<'_>> {
    begin_as(pool, TENANT).await
}

fn id(value: &str) -> Uuid {
    value.parse().unwrap()
}

fn principal(value: &str) -> PrincipalId {
    value.parse().unwrap()
}

fn document(value: &str) -> CloudDocumentId {
    value.parse().unwrap()
}

/// A `.figcraft`-shaped archive (as `figcraft-format` writes one) unique to this run.
fn archive(label: &str) -> Vec<u8> {
    let mut buffer = Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(&mut buffer);
    let deflated = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("document.json", deflated).unwrap();
    let body = format!(r#"{{"format":"figcraft","version":1,"pages":[{{"name":"{label} {}","children":[]}}]}}"#, Uuid::new_v4());
    zip.write_all(body.as_bytes()).unwrap();
    zip.start_file("library/manifest.json", deflated).unwrap();
    zip.write_all(br#"{"id":"lib","name":"Library"}"#).unwrap();
    zip.finish().unwrap();
    buffer.into_inner()
}

/// A zip with no `document.json`.
fn archive_without_document() -> Vec<u8> {
    let mut buffer = Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(&mut buffer);
    zip.start_file("notes.txt", zip::write::SimpleFileOptions::default()).unwrap();
    zip.write_all(Uuid::new_v4().to_string().as_bytes()).unwrap();
    zip.finish().unwrap();
    buffer.into_inner()
}

async fn new_library(pool: &PgPool, team: Option<&str>, name: &str) -> TestResult<Uuid> {
    let mut transaction = begin(pool).await?;
    let library = create_library(&mut transaction, team.map(id), Some(document(SOURCE_DOCUMENT)), name).await?;
    transaction.commit().await?;
    Ok(library.id)
}

async fn publish(
    pool: &PgPool,
    store: &ObjectStore,
    library: Uuid,
    publisher: &str,
    label: &str,
) -> TestResult<Result<PublishedVersion, LibraryError>> {
    let mut transaction = begin(pool).await?;
    let published = publish_library_version(&mut transaction, store, library, principal(publisher), archive(label)).await;
    if published.is_ok() {
        transaction.commit().await?;
    } else {
        transaction.rollback().await?;
    }
    Ok(published)
}

async fn delete_objects(store: &ObjectStore, published: &[PublishedVersion]) -> TestResult {
    for version in published {
        store.delete_object(&version.object.object_key).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publishing_validates_sequences_references_and_authorizes() -> TestResult {
    let Some((pool, store)) = probe().await? else {
        return Ok(());
    };
    let tenant: TenantId = TENANT.parse()?;

    // Creation trims and bounds the name and refuses foreign, archived and unknown teams and
    // foreign or deleting source documents.
    let mut scoped = begin(&pool).await?;
    for name in ["", "   ", "tab\tname", &"x".repeat(256)] {
        assert_eq!(create_library(&mut scoped, None, None, name).await, Err(LibraryError::InvalidName), "{name:?}");
    }
    assert_eq!(create_library(&mut scoped, Some(id(OTHER_TEAM)), None, "Icons").await, Err(LibraryError::TeamNotFound));
    assert_eq!(create_library(&mut scoped, Some(Uuid::new_v4()), None, "Icons").await, Err(LibraryError::TeamNotFound));
    assert_eq!(create_library(&mut scoped, Some(id(ARCHIVED_TEAM)), None, "Icons").await, Err(LibraryError::TeamNotActive));
    assert_eq!(create_library(&mut scoped, None, Some(document(OTHER_DOCUMENT)), "Icons").await, Err(LibraryError::DocumentNotFound));
    assert_eq!(create_library(&mut scoped, None, Some(document(DELETING_DOCUMENT)), "Icons").await, Err(LibraryError::DocumentNotActive));
    let created = create_library(&mut scoped, Some(id(OWNING_TEAM)), Some(document(SOURCE_DOCUMENT)), "  Icons  ").await?;
    assert_eq!(created.name, "Icons");
    assert_eq!(created.state, LibraryState::Active);
    assert_eq!(created.tenant_id, tenant);
    assert_eq!(created.owning_team_id, Some(id(OWNING_TEAM)));
    assert_eq!(created.source_document_id, Some(document(SOURCE_DOCUMENT)));
    scoped.commit().await?;
    let library = created.id;
    let mut published = Vec::new();

    // The first publish is version 1: a verified library object at its content-addressed key,
    // read back intact, with one `library_version` reference.
    let first = publish(&pool, &store, library, TEAM_EDITOR, "first").await??;
    published.push(first.clone());
    assert_eq!(first.version.version, 1);
    assert_eq!(first.version.library_id, library);
    assert_eq!(first.version.published_by, principal(TEAM_EDITOR));
    assert_eq!(first.object.kind, ObjectKind::Library);
    assert_eq!(first.object.state, ObjectState::Verified);
    assert_eq!(first.object.media_type, "application/vnd.figcraft.library");
    assert_eq!(first.object.sha256, first.version.sha256);
    assert_eq!(first.object.object_key, keys::library(tenant, library, 1, &first.version.sha256));
    let stored = store.get_object(&first.object.object_key, u64::try_from(first.object.size_bytes)?).await?;
    assert_eq!(<[u8; 32]>::from(Sha256::digest(&stored)), first.version.sha256);
    let mut scoped = begin(&pool).await?;
    assert_eq!(reference_count(&mut scoped, first.object.id).await?, ReferenceCount { references: 1, holds: 0 });
    assert_eq!(list_references(&mut scoped, first.object.id, 10).await?, vec![ReferenceOwner::Owner(OwnerKind::LibraryVersion, first.version.id)]);
    assert_eq!(get_version(&mut scoped, first.version.id).await?, Some(first.version.clone()));

    // Invalid archives are refused before any SQL, and take no version number.
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&[0; 32]);
    for (bytes, rejection) in [
        (Vec::new(), Rejection::Empty),
        (b"not a zip at all".to_vec(), Rejection::MediaMismatch),
        (png, Rejection::MediaMismatch),
        (archive_without_document(), Rejection::MissingMember),
    ] {
        let refused = publish_library_version(&mut scoped, &store, library, principal(TEAM_EDITOR), bytes).await;
        assert!(matches!(refused, Err(LibraryError::Invalid(_))), "{refused:?}");
        if rejection != Rejection::MediaMismatch {
            assert_eq!(refused, Err(LibraryError::Invalid(rejection)));
        }
    }
    let mut truncated = archive("truncated");
    truncated.truncate(truncated.len() / 2);
    assert!(matches!(publish_library_version(&mut scoped, &store, library, principal(TEAM_EDITOR), truncated).await, Err(LibraryError::Invalid(_))));
    assert_eq!(list_versions(&mut scoped, library, None, MAX_LIBRARY_PAGE).await?.len(), 1);
    scoped.rollback().await?;

    // Team and tenant mismatches: a team viewer, a member of a team without a grant, a tenant
    // editor of a team-owned library and another tenant's principal may not publish; a tenant
    // admin may.
    for refused in [TEAM_VIEWER, GRANTED_EDITOR, TENANT_EDITOR, OUTSIDER] {
        assert_eq!(publish(&pool, &store, library, refused, "refused").await?, Err(LibraryError::NotPublisher), "{refused}");
    }
    let second = publish(&pool, &store, library, TENANT_ADMIN, "second").await??;
    published.push(second.clone());
    assert_eq!(second.version.version, 2);

    // Grants: idempotent upsert of the role, refused for foreign and archived teams. Only a
    // `publisher` grant lets the granted team's editor publish.
    let mut scoped = begin(&pool).await?;
    assert_eq!(grant_library_to_team(&mut scoped, library, id(GRANTED_TEAM), LibraryRole::Viewer).await?, Change::Applied);
    assert_eq!(grant_library_to_team(&mut scoped, library, id(GRANTED_TEAM), LibraryRole::Viewer).await?, Change::AlreadyApplied);
    assert_eq!(grant_library_to_team(&mut scoped, library, id(OTHER_TEAM), LibraryRole::Viewer).await, Err(LibraryError::TeamNotFound));
    assert_eq!(grant_library_to_team(&mut scoped, library, id(ARCHIVED_TEAM), LibraryRole::Viewer).await, Err(LibraryError::TeamNotActive));
    assert_eq!(grant_library_to_team(&mut scoped, Uuid::new_v4(), id(GRANTED_TEAM), LibraryRole::Viewer).await, Err(LibraryError::NotFound));
    scoped.commit().await?;
    assert_eq!(publish(&pool, &store, library, GRANTED_EDITOR, "viewer grant").await?, Err(LibraryError::NotPublisher));
    let mut scoped = begin(&pool).await?;
    assert_eq!(grant_library_to_team(&mut scoped, library, id(GRANTED_TEAM), LibraryRole::Publisher).await?, Change::Applied);
    let grants = list_grants(&mut scoped, library, MAX_LIBRARY_PAGE).await?;
    assert_eq!(grants.len(), 1);
    assert_eq!((grants[0].team_id, grants[0].role), (id(GRANTED_TEAM), LibraryRole::Publisher));
    assert_eq!(list_grants(&mut scoped, library, 0).await, Err(LibraryError::InvalidLimit));
    scoped.commit().await?;
    let third = publish(&pool, &store, library, GRANTED_EDITOR, "third").await??;
    published.push(third.clone());
    assert_eq!(third.version.version, 3);
    assert_ne!(third.object.object_key, second.object.object_key);

    // Listing is newest first, keyset-paginated and bounded.
    let mut scoped = begin(&pool).await?;
    let numbers = |versions: Vec<LibraryVersion>| versions.into_iter().map(|version| version.version).collect::<Vec<_>>();
    assert_eq!(numbers(list_versions(&mut scoped, library, None, MAX_LIBRARY_PAGE).await?), vec![3, 2, 1]);
    assert_eq!(numbers(list_versions(&mut scoped, library, Some(3), 1).await?), vec![2]);
    assert_eq!(numbers(list_versions(&mut scoped, library, Some(1), 10).await?), Vec::<u64>::new());
    assert_eq!(numbers(list_versions(&mut scoped, library, Some(u64::MAX), 10).await?), vec![3, 2, 1]);
    assert_eq!(list_versions(&mut scoped, library, None, 0).await, Err(LibraryError::InvalidLimit));
    assert_eq!(list_versions(&mut scoped, library, None, MAX_LIBRARY_PAGE + 1).await, Err(LibraryError::InvalidLimit));

    // Revocation is idempotent and removes the publisher path again.
    assert_eq!(revoke_library_grant(&mut scoped, library, id(GRANTED_TEAM)).await?, Change::Applied);
    assert_eq!(revoke_library_grant(&mut scoped, library, id(GRANTED_TEAM)).await?, Change::AlreadyApplied);
    scoped.commit().await?;
    assert_eq!(publish(&pool, &store, library, GRANTED_EDITOR, "revoked").await?, Err(LibraryError::NotPublisher));

    // Another tenant sees none of it and cannot publish into it, grant on it or use its versions.
    let mut foreign = begin_as(&pool, OTHER_TENANT).await?;
    assert_eq!(get_library(&mut foreign, library).await?, None);
    assert_eq!(get_version(&mut foreign, first.version.id).await?, None);
    assert_eq!(list_versions(&mut foreign, library, None, 10).await?, Vec::new());
    assert_eq!(publish_library_version(&mut foreign, &store, library, principal(OUTSIDER), archive("foreign")).await, Err(LibraryError::NotFound));
    assert_eq!(grant_library_to_team(&mut foreign, library, id(OTHER_TEAM), LibraryRole::Viewer).await, Err(LibraryError::NotFound));
    assert_eq!(record_library_usage(&mut foreign, document(OTHER_DOCUMENT), first.version.id).await, Err(LibraryError::VersionNotFound));
    assert_eq!(reference_count(&mut foreign, first.object.id).await?, ReferenceCount::default());
    foreign.rollback().await?;

    delete_objects(&store, &published).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn usages_upsert_and_archived_libraries_freeze() -> TestResult {
    let Some((pool, store)) = probe().await? else {
        return Ok(());
    };
    // A library with no owning team: a tenant editor may publish, a tenant viewer (even one in an
    // unrelated team) may not.
    let library = new_library(&pool, None, "Tokens").await?;
    assert_eq!(publish(&pool, &store, library, TEAM_EDITOR, "team-less").await?, Err(LibraryError::NotPublisher));
    let first = publish(&pool, &store, library, TENANT_EDITOR, "v1").await??;
    let second = publish(&pool, &store, library, TENANT_EDITOR, "v2").await??;
    let published = vec![first.clone(), second.clone()];
    let (v1, v2) = (first.version.id, second.version.id);

    let mut scoped = begin(&pool).await?;
    let consumer = document(CONSUMER);
    let source = document(SOURCE_DOCUMENT);
    assert_eq!(record_library_usage(&mut scoped, consumer, v1).await?, UsageChange::Recorded);
    assert_eq!(record_library_usage(&mut scoped, consumer, v1).await?, UsageChange::AlreadyRecorded);
    assert_eq!(record_library_usage(&mut scoped, consumer, v2).await?, UsageChange::Switched { previous: v1 });
    assert_eq!(record_library_usage(&mut scoped, source, v2).await?, UsageChange::Recorded);
    assert_eq!(
        usage_of_library(&mut scoped, consumer, library).await?,
        Some(LibraryUsage { document_id: consumer, library_id: library, library_version_id: v2 })
    );
    assert_eq!(usages_of_version(&mut scoped, v1, None, MAX_LIBRARY_PAGE).await?, Vec::new());
    let on_v2 = usages_of_version(&mut scoped, v2, None, MAX_LIBRARY_PAGE).await?;
    assert_eq!(on_v2.iter().map(|usage| usage.document_id).collect::<BTreeSet<_>>(), [consumer, source].into_iter().collect());
    let page = usages_of_version(&mut scoped, v2, None, 1).await?;
    assert_eq!(page.len(), 1);
    let rest = usages_of_version(&mut scoped, v2, Some(page[0].document_id), MAX_LIBRARY_PAGE).await?;
    assert_eq!(rest.len(), 1);
    assert_ne!(rest[0].document_id, page[0].document_id);
    assert_eq!(usages_of_version(&mut scoped, v2, None, 0).await, Err(LibraryError::InvalidLimit));

    // Inactive, foreign and unknown documents and unknown versions are refused.
    assert_eq!(record_library_usage(&mut scoped, document(ARCHIVED_DOCUMENT), v2).await, Err(LibraryError::DocumentNotActive));
    assert_eq!(record_library_usage(&mut scoped, document(DELETING_DOCUMENT), v2).await, Err(LibraryError::DocumentNotActive));
    assert_eq!(record_library_usage(&mut scoped, document(OTHER_DOCUMENT), v2).await, Err(LibraryError::DocumentNotFound));
    assert_eq!(record_library_usage(&mut scoped, CloudDocumentId::random(), v2).await, Err(LibraryError::DocumentNotFound));
    assert_eq!(record_library_usage(&mut scoped, consumer, Uuid::new_v4()).await, Err(LibraryError::VersionNotFound));

    // Usage removal is idempotent.
    assert_eq!(remove_library_usage(&mut scoped, source, library).await?, Change::Applied);
    assert_eq!(remove_library_usage(&mut scoped, source, library).await?, Change::AlreadyApplied);
    assert_eq!(usage_of_library(&mut scoped, source, library).await?, None);
    scoped.commit().await?;

    // Archiving freezes the library: no versions, grants, new usages or switches, but the
    // existing usage stays and re-recording it is a no-op.
    let mut scoped = begin(&pool).await?;
    assert_eq!(archive_library(&mut scoped, library).await?, Change::Applied);
    assert_eq!(archive_library(&mut scoped, library).await?, Change::AlreadyApplied);
    assert_eq!(get_library(&mut scoped, library).await?.map(|library| library.state), Some(LibraryState::Archived));
    assert_eq!(
        publish_library_version(&mut scoped, &store, library, principal(TENANT_EDITOR), archive("archived")).await,
        Err(LibraryError::Archived)
    );
    assert_eq!(grant_library_to_team(&mut scoped, library, id(GRANTED_TEAM), LibraryRole::Viewer).await, Err(LibraryError::Archived));
    assert_eq!(record_library_usage(&mut scoped, consumer, v2).await?, UsageChange::AlreadyRecorded);
    assert_eq!(record_library_usage(&mut scoped, consumer, v1).await, Err(LibraryError::Archived));
    assert_eq!(record_library_usage(&mut scoped, source, v2).await, Err(LibraryError::Archived));
    assert_eq!(usages_of_version(&mut scoped, v2, None, MAX_LIBRARY_PAGE).await?.len(), 1);
    assert_eq!(revoke_library_grant(&mut scoped, library, id(GRANTED_TEAM)).await?, Change::AlreadyApplied);
    assert_eq!(archive_library(&mut scoped, Uuid::new_v4()).await, Err(LibraryError::NotFound));
    scoped.commit().await?;

    // A deleted library refuses archiving, publishing and usage changes (rolled back afterwards).
    let mut scoped = begin(&pool).await?;
    sqlx::query("UPDATE libraries SET state = 'deleted' WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(TENANT)
        .bind(library.to_string())
        .execute(scoped.connection())
        .await?;
    assert_eq!(archive_library(&mut scoped, library).await, Err(LibraryError::Deleted));
    assert_eq!(publish_library_version(&mut scoped, &store, library, principal(TENANT_EDITOR), archive("deleted")).await, Err(LibraryError::Deleted));
    assert_eq!(record_library_usage(&mut scoped, consumer, v1).await, Err(LibraryError::Deleted));
    scoped.rollback().await?;

    // Another tenant cannot archive it or see its usages.
    let mut foreign = begin_as(&pool, OTHER_TENANT).await?;
    assert_eq!(archive_library(&mut foreign, library).await, Err(LibraryError::NotFound));
    assert_eq!(usages_of_version(&mut foreign, v2, None, MAX_LIBRARY_PAGE).await?, Vec::new());
    assert_eq!(remove_library_usage(&mut foreign, consumer, library).await?, Change::AlreadyApplied);
    foreign.rollback().await?;

    delete_objects(&store, &published).await
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
            return Err("publisher never blocked on the library row lock".into());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_publishers_take_consecutive_versions() -> TestResult {
    let Some((pool, store)) = probe().await? else {
        return Ok(());
    };
    let library = new_library(&pool, Some(OWNING_TEAM), "Race").await?;
    let mut published = Vec::new();

    // Ordered: the second publisher blocks on the first's library row lock and, once the first
    // commits, takes the next number.
    let mut holder = begin(&pool).await?;
    let held = publish_library_version(&mut holder, &store, library, principal(TEAM_EDITOR), archive("holder")).await?;
    let (pid_sender, pid_receiver) = tokio::sync::oneshot::channel();
    let (waiter_pool, waiter_store) = (pool.clone(), store.clone());
    let waiting = tokio::spawn(async move {
        let mut waiter = begin(&waiter_pool).await.map_err(|error| error.to_string())?;
        let pid = backend_pid(&mut waiter).await.map_err(|error| error.to_string())?;
        pid_sender.send(pid).map_err(|_| "pid receiver dropped".to_string())?;
        let result = publish_library_version(&mut waiter, &waiter_store, library, principal(TENANT_ADMIN), archive("waiter")).await;
        if result.is_ok() {
            waiter.commit().await.map_err(|error| error.to_string())?;
        }
        Ok::<_, String>(result)
    });
    let waiter_pid = timeout(BOUND, pid_receiver).await??;
    wait_until_blocked(&pool, waiter_pid).await?;
    holder.commit().await?;
    let followed = timeout(BOUND, waiting).await???.map_err(|error| format!("waiter: {error:?}"))?;
    assert_eq!((held.version.version, followed.version.version), (1, 2));
    published.extend([held, followed]);

    // Free-running: every racer commits a distinct, dense version number.
    let barrier = std::sync::Arc::new(Barrier::new(RACERS));
    let mut racers = Vec::new();
    for racer in 0..RACERS {
        let (pool, store, barrier) = (pool.clone(), store.clone(), barrier.clone());
        racers.push(tokio::spawn(async move {
            barrier.wait().await;
            publish(&pool, &store, library, TEAM_EDITOR, &format!("racer {racer}")).await.map_err(|error| error.to_string())
        }));
    }
    for racer in racers {
        published.push(timeout(BOUND, racer).await???.map_err(|error| format!("racer: {error:?}"))?);
    }
    let numbers: BTreeSet<u64> = published.iter().map(|version| version.version.version).collect();
    let expected = u64::try_from(RACERS + 2)?;
    assert_eq!(numbers, (1..=expected).collect());
    let keys: BTreeSet<&str> = published.iter().map(|version| version.object.object_key.as_str()).collect();
    assert_eq!(keys.len(), published.len());

    let mut scoped = begin(&pool).await?;
    let listed = list_versions(&mut scoped, library, None, MAX_LIBRARY_PAGE).await?;
    assert_eq!(listed.iter().map(|version| version.version).collect::<Vec<_>>(), (1..=expected).rev().collect::<Vec<_>>());
    for version in &published {
        assert_eq!(reference_count(&mut scoped, version.object.id).await?.references, 1);
    }
    scoped.rollback().await?;

    delete_objects(&store, &published).await
}

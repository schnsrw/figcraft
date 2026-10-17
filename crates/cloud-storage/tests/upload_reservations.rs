use std::time::Duration;

use figcraft_cloud_domain::{PrincipalId, TenantId, Uuid};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::object_metadata::{ObjectKind, ObjectState, Rejection, keys};
use figcraft_cloud_storage::objects::{MAX_PRESIGN_TTL, ObjectStore, ObjectStoreError, ObjectStoreSettings};
use figcraft_cloud_storage::uploads::{
    Completion, MAX_ASSET_BYTES, MAX_EXPIRE_BATCH, MAX_OPEN_RESERVATIONS_PER_PRINCIPAL, ReservationState, ReservedUpload, UploadError,
    complete_upload, expire_reservations, get_reservation, reserve_upload,
};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgPool, PgPoolOptions};

/// Fixtures and raw inspection queries run as the administrator: row-level security (S4-014)
/// hides every tenant row from the runtime role outside a tenant transaction. Repository calls
/// still run as the runtime role.
async fn admin_pool() -> Result<sqlx::PgPool, sqlx::Error> {
    let file = std::env::var("FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE")
        .map_err(|_| sqlx::Error::Configuration("FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE is unset".into()))?;
    let url = std::fs::read_to_string(file)?;
    sqlx::postgres::PgPoolOptions::new().max_connections(1).connect(url.trim_end()).await
}

const TENANT: &str = "a2000000-0000-4000-8000-000000000001";
const OTHER_TENANT: &str = "a2000000-0000-4000-8000-000000000002";
const PRINCIPAL: &str = "a2000000-0000-4000-8000-000000000009";

type Probe = (PgPool, ObjectStoreSettings);

async fn probe() -> Result<Option<Probe>, Box<dyn std::error::Error>> {
    let (Ok(database_url_file), Ok(s3_directory)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR")) else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; upload reservation probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    let pool = PgPoolOptions::new().max_connections(2).connect(database_url.trim_end()).await?;
    let read = |name: &str| -> Result<String, Box<dyn std::error::Error>> {
        Ok(std::fs::read_to_string(std::path::Path::new(&s3_directory).join(name))?.trim_end().to_string())
    };
    let settings = ObjectStoreSettings {
        endpoint: read("s3_endpoint")?,
        region: "us-east-1".into(),
        bucket: "figcraft".into(),
        access_key: read("s3_access_key")?,
        secret_key: read("s3_secret_key")?,
        path_style: true,
    };
    Ok(Some((pool, settings)))
}

/// A client like a browser's: it holds no credentials and only follows presigned URLs.
/// `reqwest::Client::new()` panics under `rustls-no-provider`, so the provider is explicit.
fn presigned_client() -> Result<reqwest::Client, Box<dyn std::error::Error>> {
    let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    Ok(reqwest::Client::builder().tls_backend_preconfigured(tls).build()?)
}

/// A valid 1x1 greyscale PNG unique to this run (a `tEXt` chunk carries a random id), so reruns
/// never collide on content-addressed keys and completion's content validation accepts it.
fn unique_bytes(label: &str) -> Vec<u8> {
    fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        png.extend_from_slice(&u32::try_from(data.len()).unwrap_or(u32::MAX).to_be_bytes());
        let mut crc = crc32fast::Hasher::new();
        crc.update(kind);
        crc.update(data);
        png.extend_from_slice(kind);
        png.extend_from_slice(data);
        png.extend_from_slice(&crc.finalize().to_be_bytes());
    }
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut png, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 0, 0, 0, 0]);
    chunk(&mut png, b"tEXt", format!("Comment\0figcraft upload probe {label} {}", Uuid::new_v4()).as_bytes());
    // One stored zlib block holding the scanline: filter byte 0, then the grey pixel 0.
    chunk(&mut png, b"IDAT", &[0x78, 0x01, 0x01, 0x02, 0x00, 0xfd, 0xff, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01]);
    chunk(&mut png, b"IEND", &[]);
    png
}

/// PUT `body` through a reservation's presigned URL with the headers it signed.
async fn upload(client: &reqwest::Client, reserved: &ReservedUpload, body: Vec<u8>) -> Result<bool, Box<dyn std::error::Error>> {
    let mut request = client.put(&reserved.url).body(body);
    for (name, value) in &reserved.headers {
        request = request.header(*name, *value);
    }
    Ok(request.send().await?.status().is_success())
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn size(bytes: &[u8]) -> i64 {
    i64::try_from(bytes.len()).unwrap_or(i64::MAX)
}

/// Force a committed reservation's expiry into the past (the table requires it to stay after
/// `created_at`, which is the reserving transaction's start).
async fn backdate(_pool: &PgPool, id: Uuid) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query("UPDATE upload_reservations SET expires_at = created_at + interval '1 microsecond' WHERE id = $1::uuid")
        .bind(id.to_string())
        .execute(&admin_pool().await?)
        .await?;
    Ok(())
}

#[tokio::test]
async fn reservations_bind_kind_digest_size_type_and_expiry_to_one_staging_put() -> Result<(), Box<dyn std::error::Error>> {
    let Some((pool, settings)) = probe().await? else {
        return Ok(());
    };
    let store = ObjectStore::new(&settings)?;
    let client = presigned_client()?;
    let tenant: TenantId = TENANT.parse()?;
    let principal: PrincipalId = PRINCIPAL.parse()?;
    let bytes = unique_bytes("reserve");

    // A reservation records exactly what was asked for and signs one PUT for its staging key,
    // valid for the reservation's lifetime.
    let mut scoped = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let ttl = Duration::from_secs(120);
    let reserved = reserve_upload(&mut scoped, &store, principal, ObjectKind::Asset, digest(&bytes), size(&bytes), "image/png", ttl).await?;
    scoped.commit().await?;
    let reservation = &reserved.reservation;
    assert_eq!(reserved.method, "PUT");
    assert_eq!(reserved.ttl, ttl);
    assert_eq!(reservation.staging_key, keys::staging(tenant, reservation.id));
    assert_eq!((reservation.tenant_id, reservation.principal_id, reservation.kind), (tenant, principal, ObjectKind::Asset));
    assert_eq!((reservation.expected_sha256, reservation.expected_size), (digest(&bytes), size(&bytes)));
    assert_eq!((reservation.media_type.as_str(), reservation.state, reservation.expired), ("image/png", ReservationState::Reserved, false));
    assert_eq!(reservation.object_id, None);
    let now = i64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs())?;
    assert!((now + 110..=now + 130).contains(&reservation.expires_at_unix), "{} vs {now}", reservation.expires_at_unix);
    assert!(reserved.url.contains(&format!("/figcraft/{}?", reservation.staging_key)), "{}", reserved.url);
    assert!(reserved.url.contains("X-Amz-Expires=120"), "{}", reserved.url);
    assert!(!reserved.url.contains(&settings.secret_key));

    // The URL uploads to the staging key and nowhere else.
    assert!(upload(&client, &reserved, bytes.clone()).await?);
    assert_eq!(store.get_object(&reservation.staging_key, 1024).await?, bytes);
    let elsewhere =
        ReservedUpload { url: reserved.url.replacen(&reservation.staging_key, &keys::asset(tenant, &digest(&bytes)), 1), ..reserved.clone() };
    assert!(!upload(&client, &elsewhere, bytes.clone()).await?);
    // The staging tag is signed in, so an upload that drops it is refused.
    assert_eq!(reserved.headers, vec![("x-amz-tagging", "figcraft-staging=true")]);
    assert!(!client.put(&reserved.url).body(bytes.clone()).send().await?.status().is_success());
    assert_eq!(store.get_object(&keys::asset(tenant, &digest(&bytes)), 1024).await, Err(ObjectStoreError::NotFound));

    // Validity is capped to the presign maximum.
    let mut capped = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let long = reserve_upload(&mut capped, &store, principal, ObjectKind::Asset, [7; 32], 1, "image/webp", Duration::from_secs(86_400)).await?;
    assert_eq!(long.ttl, MAX_PRESIGN_TTL);
    assert!(long.url.contains("X-Amz-Expires=900"), "{}", long.url);
    capped.rollback().await?;

    // Bad sizes, kinds, types and TTLs are refused before any SQL, so the transaction stays usable.
    let mut refused = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let digest_value = [1; 32];
    for (kind, expected_size, media_type, ttl, error) in [
        (ObjectKind::Asset, 0, "image/png", ttl, UploadError::InvalidSize),
        (ObjectKind::Asset, -5, "image/png", ttl, UploadError::InvalidSize),
        (ObjectKind::Asset, MAX_ASSET_BYTES + 1, "image/png", ttl, UploadError::InvalidSize),
        (ObjectKind::Asset, 10, "image/svg+xml", ttl, UploadError::UnsupportedMediaType),
        (ObjectKind::Asset, 10, "Image/PNG", ttl, UploadError::UnsupportedMediaType),
        (ObjectKind::Snapshot, 10, "image/png", ttl, UploadError::UnsupportedMediaType),
        (ObjectKind::Export, 10, "application/octet-stream", ttl, UploadError::UnsupportedKind),
        (ObjectKind::Thumbnail, 10, "image/webp", ttl, UploadError::UnsupportedKind),
        (ObjectKind::Asset, 10, "image/png", Duration::from_millis(10), UploadError::InvalidTtl),
    ] {
        assert_eq!(reserve_upload(&mut refused, &store, principal, kind, digest_value, expected_size, media_type, ttl).await, Err(error));
    }
    // An unknown principal is refused by the table.
    assert_eq!(
        reserve_upload(&mut refused, &store, PrincipalId::random(), ObjectKind::Asset, digest_value, 1, "image/png", ttl).await,
        Err(UploadError::Constraint)
    );
    refused.rollback().await?;

    // One principal cannot hold unbounded open reservations.
    let mut flood = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let mut made = 0;
    let outcome = loop {
        match reserve_upload(&mut flood, &store, principal, ObjectKind::Asset, digest_value, 1, "image/png", ttl).await {
            Ok(_) if made <= MAX_OPEN_RESERVATIONS_PER_PRINCIPAL => made += 1,
            other => break other.map(|_| ()),
        }
    };
    assert_eq!(outcome, Err(UploadError::TooManyReservations));
    assert!(made < MAX_OPEN_RESERVATIONS_PER_PRINCIPAL);
    flood.rollback().await?;

    // Another tenant cannot see the reservation.
    let other: TenantId = OTHER_TENANT.parse()?;
    let mut other_scope = begin_tenant_transaction(&pool, other).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    assert_eq!(get_reservation(&mut other_scope, reservation.id).await?, None);
    assert_eq!(expire_reservations(&mut other_scope, 10).await?, Vec::<String>::new());
    other_scope.rollback().await?;

    // The sweep expires only overdue `reserved` rows and returns their staging keys for deletion.
    backdate(&pool, reservation.id).await?;
    let mut fresh_scope = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let fresh = reserve_upload(&mut fresh_scope, &store, principal, ObjectKind::Asset, digest_value, 1, "image/png", ttl).await?;
    fresh_scope.commit().await?;
    let mut sweep = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    assert_eq!(expire_reservations(&mut sweep, 0).await?, Vec::<String>::new());
    let expired = expire_reservations(&mut sweep, i64::MAX).await?;
    assert!(expired.contains(&reservation.staging_key), "{expired:?}");
    assert!(!expired.contains(&fresh.reservation.staging_key));
    sweep.commit().await?;
    for key in &expired {
        store.delete_object(key).await?;
    }
    assert_eq!(store.get_object(&reservation.staging_key, 1024).await, Err(ObjectStoreError::NotFound));
    let mut check = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let swept = get_reservation(&mut check, reservation.id).await?.ok_or("reservation vanished")?;
    assert_eq!((swept.state, swept.expired), (ReservationState::Expired, true));
    assert_eq!(get_reservation(&mut check, fresh.reservation.id).await?.map(|row| row.state), Some(ReservationState::Reserved));
    assert!(!expire_reservations(&mut check, 10).await?.contains(&reservation.staging_key));
    check.rollback().await?;

    pool.close().await;
    Ok(())
}

/// Reserve an asset upload for `bytes` (as claimed) and commit it.
async fn reserve_asset(pool: &PgPool, store: &ObjectStore, claimed: &[u8], expected_size: i64) -> Result<ReservedUpload, Box<dyn std::error::Error>> {
    let tenant: TenantId = TENANT.parse()?;
    let principal: PrincipalId = PRINCIPAL.parse()?;
    let mut scoped = begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let reserved =
        reserve_upload(&mut scoped, store, principal, ObjectKind::Asset, digest(claimed), expected_size, "image/png", Duration::from_secs(300))
            .await?;
    scoped.commit().await?;
    Ok(reserved)
}

/// Complete `id` in `tenant` and commit, or roll back on error.
async fn complete(
    pool: &PgPool,
    store: &ObjectStore,
    tenant: TenantId,
    id: Uuid,
) -> Result<Result<Completion, UploadError>, Box<dyn std::error::Error>> {
    let mut scoped = begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let outcome = complete_upload(&mut scoped, store, id, PRINCIPAL.parse()?, "figcraft-cloud test").await;
    if outcome.is_ok() {
        scoped.commit().await?
    } else {
        scoped.rollback().await?
    }
    Ok(outcome)
}

async fn state_of(pool: &PgPool, id: Uuid) -> Result<Option<ReservationState>, Box<dyn std::error::Error>> {
    let mut scoped = begin_tenant_transaction(pool, TENANT.parse()?).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let state = get_reservation(&mut scoped, id).await?.map(|row| row.state);
    scoped.rollback().await?;
    Ok(state)
}

#[tokio::test]
async fn completion_verifies_staged_bytes_and_promotes_them_to_one_content_addressed_object() -> Result<(), Box<dyn std::error::Error>> {
    let Some((pool, settings)) = probe().await? else {
        return Ok(());
    };
    let store = ObjectStore::new(&settings)?;
    let client = presigned_client()?;
    let tenant: TenantId = TENANT.parse()?;
    let other: TenantId = OTHER_TENANT.parse()?;

    // Happy path: the client PUTs through the presigned URL, completion reads the staging object
    // back, writes and verifies the content-addressed key and deletes the staging object.
    let bytes = unique_bytes("happy");
    let reserved = reserve_asset(&pool, &store, &bytes, size(&bytes)).await?;
    let id = reserved.reservation.id;
    // Completing before anything is staged changes nothing.
    assert_eq!(complete(&pool, &store, tenant, id).await?, Err(UploadError::NotUploaded));
    assert_eq!(state_of(&pool, id).await?, Some(ReservationState::Reserved));
    assert!(upload(&client, &reserved, bytes.clone()).await?);
    // Another tenant cannot complete it, and that attempt leaves it untouched.
    assert_eq!(complete(&pool, &store, other, id).await?, Err(UploadError::NotFound));
    assert_eq!(complete(&pool, &store, tenant, Uuid::new_v4()).await?, Err(UploadError::NotFound));
    let mut bad_version = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    for version in ["", " x", "x\n", &"v".repeat(65)] {
        assert_eq!(complete_upload(&mut bad_version, &store, id, PRINCIPAL.parse()?, version).await, Err(UploadError::InvalidProducerVersion));
    }
    // Only the principal that reserved the upload may complete it.
    assert_eq!(
        complete_upload(&mut bad_version, &store, id, PrincipalId::random(), "figcraft-cloud test").await,
        Err(UploadError::NotReservationOwner)
    );
    bad_version.rollback().await?;
    let Completion::Promoted { reservation, object, reused } = complete(&pool, &store, tenant, id).await?? else {
        return Err("matching bytes should promote".into());
    };
    assert!(!reused);
    assert_eq!((reservation.state, reservation.object_id), (ReservationState::Promoted, Some(object.id)));
    assert_eq!(object.state, ObjectState::Verified);
    assert_eq!((object.kind, object.object_key.clone()), (ObjectKind::Asset, keys::asset(tenant, &digest(&bytes))));
    assert_eq!((object.sha256, object.size_bytes, object.media_type.as_str()), (digest(&bytes), size(&bytes), "image/png"));
    assert_eq!(object.producer_version, "figcraft-cloud test");
    assert_eq!(store.get_object(&object.object_key, 1024).await?, bytes);
    assert_eq!(store.get_object(&reserved.reservation.staging_key, 1024).await, Err(ObjectStoreError::NotFound));

    // Completing again returns the same object without touching the store.
    let Completion::Promoted { object: again, reused, .. } = complete(&pool, &store, tenant, id).await?? else {
        return Err("re-completion should return the promoted object".into());
    };
    assert!(reused);
    assert_eq!(again, object);

    // Identical content in a second reservation reuses the verified object.
    let duplicate = reserve_asset(&pool, &store, &bytes, size(&bytes)).await?;
    assert!(upload(&client, &duplicate, bytes.clone()).await?);
    let Completion::Promoted { reservation: deduped, object: shared, reused } = complete(&pool, &store, tenant, duplicate.reservation.id).await??
    else {
        return Err("duplicate content should promote".into());
    };
    assert!(reused);
    assert_eq!((shared.id, deduped.object_id), (object.id, Some(object.id)));
    assert_eq!(store.get_object(&duplicate.reservation.staging_key, 1024).await, Err(ObjectStoreError::NotFound));

    // Bytes that do not hash to the reservation are rejected and the staging object deleted.
    let claimed = unique_bytes("claimed");
    let mut altered = claimed.clone();
    if let Some(first) = altered.first_mut() {
        *first ^= 1;
    }
    let wrong = reserve_asset(&pool, &store, &claimed, size(&claimed)).await?;
    assert!(upload(&client, &wrong, altered).await?);
    let outcome = complete(&pool, &store, tenant, wrong.reservation.id).await??;
    assert!(
        matches!(&outcome, Completion::Rejected { reason: Rejection::ChecksumMismatch, reservation } if reservation.state == ReservationState::Rejected && reservation.object_id.is_none())
    );
    assert_eq!(store.get_object(&wrong.reservation.staging_key, 1024).await, Err(ObjectStoreError::NotFound));
    assert_eq!(store.get_object(&keys::asset(tenant, &digest(&claimed)), 1024).await, Err(ObjectStoreError::NotFound));
    // A rejected reservation stays rejected even once the right bytes appear.
    assert!(upload(&client, &wrong, claimed.clone()).await?);
    assert_eq!(complete(&pool, &store, tenant, wrong.reservation.id).await?, Err(UploadError::InvalidState));
    store.delete_object(&wrong.reservation.staging_key).await?;

    // Bytes that match the reservation's digest and size but are not the declared image fail
    // content validation: rejected, staging deleted, nothing promoted.
    let not_an_image = format!("not a png {}", Uuid::new_v4()).into_bytes();
    let invalid = reserve_asset(&pool, &store, &not_an_image, size(&not_an_image)).await?;
    assert!(upload(&client, &invalid, not_an_image.clone()).await?);
    let outcome = complete(&pool, &store, tenant, invalid.reservation.id).await??;
    assert!(matches!(&outcome, Completion::Invalid { reservation, .. } if reservation.state == ReservationState::Rejected), "{outcome:?}");
    assert_eq!(store.get_object(&invalid.reservation.staging_key, 1024).await, Err(ObjectStoreError::NotFound));
    assert_eq!(store.get_object(&keys::asset(tenant, &digest(&not_an_image)), 1024).await, Err(ObjectStoreError::NotFound));

    // Oversize and short uploads are rejected on size; the read is bounded by the reservation.
    for (label, uploaded_extra) in [("oversize", 1usize), ("short", 0usize)] {
        let body = unique_bytes(label);
        let declared = if uploaded_extra == 1 { size(&body) - 1 } else { size(&body) + 1 };
        let reservation = reserve_asset(&pool, &store, &body, declared).await?;
        assert!(upload(&client, &reservation, body).await?);
        let outcome = complete(&pool, &store, tenant, reservation.reservation.id).await??;
        assert!(matches!(outcome, Completion::Rejected { reason: Rejection::SizeMismatch, .. }), "{label}: {outcome:?}");
        assert_eq!(store.get_object(&reservation.reservation.staging_key, 1024).await, Err(ObjectStoreError::NotFound));
    }

    // An expired reservation cannot be completed, before or after the sweep.
    let late_bytes = unique_bytes("late");
    let late = reserve_asset(&pool, &store, &late_bytes, size(&late_bytes)).await?;
    assert!(upload(&client, &late, late_bytes.clone()).await?);
    backdate(&pool, late.reservation.id).await?;
    assert_eq!(complete(&pool, &store, tenant, late.reservation.id).await?, Err(UploadError::Expired));
    let mut sweep = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let expired = expire_reservations(&mut sweep, MAX_EXPIRE_BATCH).await?;
    sweep.commit().await?;
    assert!(expired.contains(&late.reservation.staging_key), "{expired:?}");
    for key in &expired {
        store.delete_object(key).await?;
    }
    assert_eq!(complete(&pool, &store, tenant, late.reservation.id).await?, Err(UploadError::Expired));
    assert_eq!(store.get_object(&keys::asset(tenant, &digest(&late_bytes)), 1024).await, Err(ObjectStoreError::NotFound));

    // A store outage changes nothing, so the completion can be retried.
    let retry_bytes = unique_bytes("retry");
    let retry = reserve_asset(&pool, &store, &retry_bytes, size(&retry_bytes)).await?;
    assert!(upload(&client, &retry, retry_bytes.clone()).await?);
    let unreachable = ObjectStore::new(&ObjectStoreSettings { endpoint: "http://127.0.0.1:9".into(), ..settings.clone() })?;
    assert_eq!(complete(&pool, &unreachable, tenant, retry.reservation.id).await?, Err(UploadError::Store(ObjectStoreError::Unreachable)));
    assert_eq!(state_of(&pool, retry.reservation.id).await?, Some(ReservationState::Reserved));
    let Completion::Promoted { object: retried, .. } = complete(&pool, &store, tenant, retry.reservation.id).await?? else {
        return Err("retry should promote".into());
    };

    // Kinds whose final key needs identities a reservation lacks are not completed here.
    let mut snapshot_scope = begin_tenant_transaction(&pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let snapshot = reserve_upload(
        &mut snapshot_scope,
        &store,
        PRINCIPAL.parse()?,
        ObjectKind::Snapshot,
        [3; 32],
        3,
        "application/vnd.figcraft.snapshot",
        Duration::from_secs(60),
    )
    .await?;
    assert_eq!(
        complete_upload(&mut snapshot_scope, &store, snapshot.reservation.id, PRINCIPAL.parse()?, "figcraft-cloud test").await,
        Err(UploadError::UnsupportedKind)
    );
    snapshot_scope.rollback().await?;

    store.delete_object(&object.object_key).await?;
    store.delete_object(&retried.object_key).await?;
    pool.close().await;
    Ok(())
}

use std::time::Duration;

use figcraft_cloud_domain::{PrincipalId, TenantId, Uuid};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::object_metadata::{ObjectKind, keys};
use figcraft_cloud_storage::objects::{MAX_PRESIGN_TTL, ObjectStore, ObjectStoreError, ObjectStoreSettings};
use figcraft_cloud_storage::uploads::{
    MAX_ASSET_BYTES, MAX_OPEN_RESERVATIONS_PER_PRINCIPAL, ReservationState, UploadError, expire_reservations, get_reservation, reserve_upload,
};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgPool, PgPoolOptions};

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

/// Bytes unique to this run, so reruns never collide on content-addressed keys.
fn unique_bytes(label: &str) -> Vec<u8> {
    format!("figcraft upload probe {label} {}", Uuid::new_v4()).into_bytes()
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn size(bytes: &[u8]) -> i64 {
    i64::try_from(bytes.len()).unwrap_or(i64::MAX)
}

/// Force a committed reservation's expiry into the past (the table requires it to stay after
/// `created_at`, which is the reserving transaction's start).
async fn backdate(pool: &PgPool, id: Uuid) -> Result<(), Box<dyn std::error::Error>> {
    sqlx::query("UPDATE upload_reservations SET expires_at = created_at + interval '1 microsecond' WHERE id = $1::uuid")
        .bind(id.to_string())
        .execute(pool)
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
    assert!(client.put(&reserved.url).body(bytes.clone()).send().await?.status().is_success());
    assert_eq!(store.get_object(&reservation.staging_key, 1024).await?, bytes);
    let elsewhere = reserved.url.replacen(&reservation.staging_key, &keys::asset(tenant, &digest(&bytes)), 1);
    assert!(!client.put(&elsewhere).body(bytes.clone()).send().await?.status().is_success());
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

use std::time::Duration;

use figcraft_cloud_domain::{PrincipalId, TenantId, Uuid};
use figcraft_cloud_storage::downloads::{DOWNLOAD_SIGNED_ACTION, DownloadError, authorize_download, presign_download};
use figcraft_cloud_storage::object_metadata::{NewObject, ObjectKind, Verification, keys, register_object, verify_object};
use figcraft_cloud_storage::objects::{MAX_PRESIGN_TTL, ObjectStore, ObjectStoreSettings};
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgPool, PgPoolOptions};

const TENANT: &str = "a4000000-0000-4000-8000-000000000001";
const OTHER_TENANT: &str = "a4000000-0000-4000-8000-000000000002";
const SUSPENDED_TENANT: &str = "a4000000-0000-4000-8000-000000000003";
const MEMBER: &str = "a4000000-0000-4000-8000-000000000009";
const SUSPENDED_MEMBER: &str = "a4000000-0000-4000-8000-00000000000a";
const OUTSIDER: &str = "a4000000-0000-4000-8000-00000000000b";
const SUSPENDED_PRINCIPAL: &str = "a4000000-0000-4000-8000-00000000000c";

type Probe = (PgPool, ObjectStoreSettings);

async fn probe() -> Result<Option<Probe>, Box<dyn std::error::Error>> {
    let (Ok(database_url_file), Ok(s3_directory)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR")) else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; presigned download probe skipped");
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

/// `reqwest::Client::new()` panics under `rustls-no-provider`, so the downloading client gets an
/// explicit `ring` configuration, as a browser-side client would have its own TLS stack.
fn http_client() -> Result<reqwest::Client, Box<dyn std::error::Error>> {
    let tls = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    Ok(reqwest::Client::builder().tls_backend_preconfigured(tls).redirect(reqwest::redirect::Policy::none()).build()?)
}

async fn scope<'a>(pool: &'a PgPool, tenant: &str) -> Result<TenantTransaction<'a>, Box<dyn std::error::Error>> {
    begin_tenant_transaction(pool, tenant.parse()?).await.map_err(|error| format!("begin tenant: {error:?}").into())
}

/// Bytes unique to this run, so reruns never collide on content-addressed keys.
fn unique_bytes(label: &str) -> Vec<u8> {
    format!("figcraft download probe {label} {}", Uuid::new_v4()).into_bytes()
}

fn new_object(tenant: TenantId, bytes: &[u8]) -> NewObject {
    let sha256: [u8; 32] = Sha256::digest(bytes).into();
    NewObject {
        id: Uuid::new_v4(),
        kind: ObjectKind::Asset,
        object_key: keys::asset(tenant, &sha256),
        sha256,
        size_bytes: i64::try_from(bytes.len()).unwrap_or(i64::MAX),
        media_type: "application/octet-stream".into(),
        producer_version: "figcraft-cloud test".into(),
    }
}

#[tokio::test]
async fn only_authorized_principals_get_short_lived_urls_for_verified_objects() -> Result<(), Box<dyn std::error::Error>> {
    let Some((pool, settings)) = probe().await? else {
        return Ok(());
    };
    let store = ObjectStore::new(&settings)?;
    let client = http_client()?;
    let tenant: TenantId = TENANT.parse()?;
    let member: PrincipalId = MEMBER.parse()?;
    let mut stored_keys = Vec::new();

    // A verified object, a pending one, a rejected one and one being deleted, committed so other
    // tenants' transactions could see them if isolation failed.
    let mut setup = scope(&pool, TENANT).await?;
    let good = unique_bytes("good");
    let verified = new_object(tenant, &good);
    register_object(&mut setup, store.bucket_name(), &verified).await?;
    store.put_object(&verified.object_key, good.clone(), &verified.media_type).await?;
    stored_keys.push(verified.object_key.clone());
    assert!(matches!(verify_object(&mut setup, &store, verified.id).await?, Some(Verification::Verified(_))));

    let pending = new_object(tenant, &unique_bytes("pending"));
    register_object(&mut setup, store.bucket_name(), &pending).await?;

    let claimed = unique_bytes("rejected");
    let rejected = new_object(tenant, &claimed);
    register_object(&mut setup, store.bucket_name(), &rejected).await?;
    store.put_object(&rejected.object_key, unique_bytes("not the claimed bytes"), &rejected.media_type).await?;
    stored_keys.push(rejected.object_key.clone());
    assert!(matches!(verify_object(&mut setup, &store, rejected.id).await?, Some(Verification::Rejected(..))));

    let doomed_bytes = unique_bytes("deleting");
    let doomed = new_object(tenant, &doomed_bytes);
    register_object(&mut setup, store.bucket_name(), &doomed).await?;
    store.put_object(&doomed.object_key, doomed_bytes, &doomed.media_type).await?;
    stored_keys.push(doomed.object_key.clone());
    assert!(matches!(verify_object(&mut setup, &store, doomed.id).await?, Some(Verification::Verified(_))));
    setup.commit().await?;

    // An active member gets a default five-minute GET URL that downloads exactly the bytes, and
    // the issued URL is audited with the principal and object.
    let mut scoped = scope(&pool, TENANT).await?;
    let grant = authorize_download(&mut scoped, member, verified.id).await?;
    let download = presign_download(&mut scoped, &store, grant, verified.id, None).await?;
    assert_eq!(download.expires_in, Duration::from_secs(300));
    assert!(download.url.contains("X-Amz-Expires=300"));
    assert_eq!(
        (download.size_bytes, download.sha256, download.media_type.as_str()),
        (verified.size_bytes, verified.sha256, "application/octet-stream")
    );
    assert!(!download.url.contains(&settings.secret_key));
    assert!(!format!("{download:?}").contains("X-Amz"));
    let response = client.get(&download.url).send().await?;
    assert!(response.status().is_success());
    assert_eq!(response.bytes().await?.as_ref(), good.as_slice());

    // The signature names one key and one method: another key or a PUT is refused, and the
    // object is unchanged.
    let other_key = download.url.replacen(&verified.object_key, &format!("{}-other", verified.object_key), 1);
    assert_ne!(other_key, download.url);
    assert!(!client.get(&other_key).send().await?.status().is_success());
    assert!(!client.put(&download.url).body(b"overwrite".to_vec()).send().await?.status().is_success());
    assert_eq!(store.get_object(&verified.object_key, 1024).await?, good);

    // TTLs are clamped to the presign maximum and refused below one second.
    let grant = authorize_download(&mut scoped, member, verified.id).await?;
    let long = presign_download(&mut scoped, &store, grant, verified.id, Some(Duration::from_secs(24 * 3600))).await?;
    assert_eq!(long.expires_in, MAX_PRESIGN_TTL);
    assert!(long.url.contains("X-Amz-Expires=900"));
    let grant = authorize_download(&mut scoped, member, verified.id).await?;
    assert_eq!(presign_download(&mut scoped, &store, grant, verified.id, Some(Duration::from_millis(400))).await, Err(DownloadError::InvalidTtl));
    let grant = authorize_download(&mut scoped, member, verified.id).await?;
    let short = presign_download(&mut scoped, &store, grant, verified.id, Some(Duration::from_secs(1))).await?;
    assert!(short.url.contains("X-Amz-Expires=1&") || short.url.ends_with("X-Amz-Expires=1"));
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!client.get(&short.url).send().await?.status().is_success(), "an expired URL must be refused");

    // A grant names one object: it cannot sign another.
    let grant = authorize_download(&mut scoped, member, verified.id).await?;
    assert_eq!(presign_download(&mut scoped, &store, grant, doomed.id, None).await, Err(DownloadError::NotAuthorized));

    // Pending, rejected and unknown objects are never authorized.
    for id in [pending.id, rejected.id, Uuid::new_v4()] {
        assert_eq!(authorize_download(&mut scoped, member, id).await, Err(DownloadError::Unavailable));
    }

    // An object deleted after authorization is refused at signing time, and afterwards at
    // authorization time.
    let grant = authorize_download(&mut scoped, member, doomed.id).await?;
    sqlx::query("UPDATE objects SET state = 'deleting', deleted_at = now() WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(TENANT)
        .bind(doomed.id.to_string())
        .execute(scoped.connection())
        .await?;
    assert_eq!(presign_download(&mut scoped, &store, grant, doomed.id, None).await, Err(DownloadError::Unavailable));
    assert_eq!(authorize_download(&mut scoped, member, doomed.id).await, Err(DownloadError::Unavailable));

    // Suspended members, outsiders and suspended principals are refused.
    for principal in [SUSPENDED_MEMBER, OUTSIDER, SUSPENDED_PRINCIPAL] {
        assert_eq!(authorize_download(&mut scoped, principal.parse()?, verified.id).await, Err(DownloadError::NotAuthorized));
    }
    let tenant_grant = authorize_download(&mut scoped, member, verified.id).await?;
    scoped.commit().await?;

    // Exactly one audit event per issued URL (default, long and short), none for refusals.
    let mut audit = scope(&pool, TENANT).await?;
    let (signed,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM audit_events WHERE tenant_id = $1::uuid AND action = $2 AND resource_kind = 'object' \
         AND resource_id = $3::uuid AND actor_principal_id = $4::uuid AND outcome = 'succeeded'",
    )
    .bind(TENANT)
    .bind(DOWNLOAD_SIGNED_ACTION)
    .bind(verified.id.to_string())
    .bind(MEMBER)
    .fetch_one(audit.connection())
    .await?;
    assert_eq!(signed, 3);
    let (doomed_signed,): (i64,) = sqlx::query_as("SELECT count(*) FROM audit_events WHERE tenant_id = $1::uuid AND resource_id = $2::uuid")
        .bind(TENANT)
        .bind(doomed.id.to_string())
        .fetch_one(audit.connection())
        .await?;
    assert_eq!(doomed_signed, 0);
    audit.rollback().await?;

    // Another tenant's transaction cannot see the object even for a principal who is a member of
    // both, and a grant issued in one tenant cannot sign in another.
    let mut other = scope(&pool, OTHER_TENANT).await?;
    assert_eq!(authorize_download(&mut other, member, verified.id).await, Err(DownloadError::Unavailable));
    assert_eq!(presign_download(&mut other, &store, tenant_grant, verified.id, None).await, Err(DownloadError::NotAuthorized));
    other.rollback().await?;

    // A suspended tenant authorizes nobody.
    let mut suspended = scope(&pool, SUSPENDED_TENANT).await?;
    assert_eq!(authorize_download(&mut suspended, member, verified.id).await, Err(DownloadError::NotAuthorized));
    suspended.rollback().await?;

    for key in stored_keys {
        store.delete_object(&key).await?;
    }
    pool.close().await;
    Ok(())
}

use std::time::Duration;

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, TenantId, Uuid};
use figcraft_cloud_storage::assets::{
    AssetError, MAX_ASSET_PAGE, attach_asset, detach_asset, find_asset, list_document_assets, presign_document_asset_download, store_asset_bytes,
};
use figcraft_cloud_storage::downloads::{DOWNLOAD_SIGNED_ACTION, DownloadError};
use figcraft_cloud_storage::object_metadata::{NewObject, ObjectKind, ObjectState, Verification, keys, register_object, verify_object};
use figcraft_cloud_storage::object_references::{Change, reference_count};
use figcraft_cloud_storage::objects::{ObjectStore, ObjectStoreSettings};
use figcraft_cloud_storage::validation::Rejection;
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgPool, PgPoolOptions};

const TENANT: &str = "a7000000-0000-4000-8000-000000000001";
const OTHER_TENANT: &str = "a7000000-0000-4000-8000-000000000002";
const MEMBER: &str = "a7000000-0000-4000-8000-000000000009";
const OUTSIDER: &str = "a7000000-0000-4000-8000-00000000000b";
const DOC_ONE: &str = "a7000000-0000-4000-8000-000000000021";
const DOC_TWO: &str = "a7000000-0000-4000-8000-000000000022";
const DOC_ARCHIVED: &str = "a7000000-0000-4000-8000-000000000023";
const DOC_DELETING: &str = "a7000000-0000-4000-8000-000000000024";
const DOC_OTHER_TENANT: &str = "a7000000-0000-4000-8000-000000000025";
const PRODUCER: &str = "figcraft-cloud asset probe";

type Probe = (PgPool, ObjectStoreSettings);

async fn probe() -> Result<Option<Probe>, Box<dyn std::error::Error>> {
    let (Ok(database_url_file), Ok(s3_directory)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR")) else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; asset probe skipped");
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

/// `reqwest::Client::new()` panics under `rustls-no-provider`, so the provider is explicit.
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

/// A valid 1x1 greyscale PNG unique to this run (a `tEXt` chunk carries a random id), so reruns
/// never collide on content-addressed keys in the shared bucket.
fn unique_png(label: &str) -> Vec<u8> {
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
    chunk(&mut png, b"tEXt", format!("Comment\0figcraft asset probe {label} {}", Uuid::new_v4()).as_bytes());
    chunk(&mut png, b"IDAT", &[0x78, 0x01, 0x01, 0x02, 0x00, 0xfd, 0xff, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01]);
    chunk(&mut png, b"IEND", &[]);
    png
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn pending_asset(tenant: TenantId, bytes: &[u8]) -> NewObject {
    let sha256 = digest(bytes);
    NewObject {
        id: Uuid::new_v4(),
        kind: ObjectKind::Asset,
        object_key: keys::asset(tenant, &sha256),
        sha256,
        size_bytes: i64::try_from(bytes.len()).unwrap_or(i64::MAX),
        media_type: "image/png".into(),
        producer_version: PRODUCER.into(),
    }
}

/// Drop document references left by an interrupted earlier run, so listings start empty.
async fn reset_document_references(pool: &PgPool) -> Result<(), Box<dyn std::error::Error>> {
    for tenant in [TENANT, OTHER_TENANT] {
        let mut scoped = scope(pool, tenant).await?;
        sqlx::query("DELETE FROM object_references WHERE tenant_id = $1::uuid AND owner_kind = 'document'")
            .bind(tenant)
            .execute(scoped.connection())
            .await?;
        scoped.commit().await?;
    }
    Ok(())
}

#[tokio::test]
async fn stored_assets_are_validated_content_addressed_and_deduplicated_per_tenant() -> Result<(), Box<dyn std::error::Error>> {
    let Some((pool, settings)) = probe().await? else {
        return Ok(());
    };
    let store = ObjectStore::new(&settings)?;
    let tenant: TenantId = TENANT.parse()?;
    let other_tenant: TenantId = OTHER_TENANT.parse()?;
    let mut stored_keys = Vec::new();

    // A new image is validated, written under its content-addressed key, read back and verified.
    let bytes = unique_png("dedupe");
    let mut scoped = scope(&pool, TENANT).await?;
    assert_eq!(find_asset(&mut scoped, &digest(&bytes)).await?, None);
    let first = store_asset_bytes(&mut scoped, &store, bytes.clone(), "image/png", PRODUCER).await?;
    stored_keys.push(first.object.object_key.clone());
    assert!(!first.reused);
    assert_eq!(first.object.object_key, keys::asset(tenant, &digest(&bytes)));
    assert_eq!((first.object.kind, first.object.state, first.object.media_type.as_str()), (ObjectKind::Asset, ObjectState::Verified, "image/png"));
    assert_eq!((first.object.sha256, first.object.producer_version.as_str()), (digest(&bytes), PRODUCER));
    assert_eq!(store.get_object(&first.object.object_key, 4096).await?, bytes);
    scoped.commit().await?;

    // The same bytes again reuse the object, and the client-side dedupe lookup finds it.
    let mut scoped = scope(&pool, TENANT).await?;
    assert_eq!(find_asset(&mut scoped, &digest(&bytes)).await?.map(|object| object.id), Some(first.object.id));
    let again = store_asset_bytes(&mut scoped, &store, bytes.clone(), "image/png", PRODUCER).await?;
    assert!(again.reused);
    assert_eq!(again.object, first.object);
    let (rows,): (i64,) = sqlx::query_as("SELECT count(*) FROM objects WHERE tenant_id = $1::uuid AND sha256 = $2")
        .bind(TENANT)
        .bind(digest(&bytes).as_slice())
        .fetch_one(scoped.connection())
        .await?;
    assert_eq!(rows, 1);
    assert_eq!(find_asset(&mut scoped, &digest(b"never stored")).await?, None);
    scoped.commit().await?;

    // Another tenant cannot see the digest and stores its own copy under its own prefix.
    let mut other = scope(&pool, OTHER_TENANT).await?;
    assert_eq!(find_asset(&mut other, &digest(&bytes)).await?, None);
    let theirs = store_asset_bytes(&mut other, &store, bytes.clone(), "image/png", PRODUCER).await?;
    stored_keys.push(theirs.object.object_key.clone());
    assert!(!theirs.reused);
    assert_ne!(theirs.object.id, first.object.id);
    assert_eq!(theirs.object.object_key, keys::asset(other_tenant, &digest(&bytes)));
    other.commit().await?;

    // A pending row already holding the key (an interrupted earlier write) is filled and verified.
    let interrupted = unique_png("interrupted");
    let mut scoped = scope(&pool, TENANT).await?;
    let pending = pending_asset(tenant, &interrupted);
    register_object(&mut scoped, store.bucket_name(), &pending).await?;
    assert_eq!(find_asset(&mut scoped, &pending.sha256).await?, None, "pending objects are never offered for dedupe");
    let filled = store_asset_bytes(&mut scoped, &store, interrupted.clone(), "image/png", PRODUCER).await?;
    stored_keys.push(filled.object.object_key.clone());
    assert_eq!((filled.object.id, filled.object.state, filled.reused), (pending.id, ObjectState::Verified, false));
    scoped.commit().await?;

    // A rejected row on the key blocks it until collection; nothing is written over it.
    let blocked = unique_png("blocked");
    let mut scoped = scope(&pool, TENANT).await?;
    let rejected = pending_asset(tenant, &blocked);
    register_object(&mut scoped, store.bucket_name(), &rejected).await?;
    assert!(matches!(verify_object(&mut scoped, &store, rejected.id).await?, Some(Verification::Rejected(..))));
    assert_eq!(find_asset(&mut scoped, &rejected.sha256).await?, None);
    assert_eq!(store_asset_bytes(&mut scoped, &store, blocked, "image/png", PRODUCER).await, Err(AssetError::ObjectConflict));
    scoped.rollback().await?;

    // Validation refuses non-image bytes and non-image types before anything is registered.
    let mut scoped = scope(&pool, TENANT).await?;
    let not_an_image = format!("plain bytes, not an image {}", Uuid::new_v4()).into_bytes();
    for (bytes, media_type, expected) in [
        (not_an_image.clone(), "image/png", AssetError::Invalid(Rejection::MediaMismatch)),
        (not_an_image.clone(), "application/octet-stream", AssetError::UnsupportedMediaType),
        (b"<svg xmlns='http://www.w3.org/2000/svg'/>".to_vec(), "image/svg+xml", AssetError::UnsupportedMediaType),
        (unique_png("mislabelled"), "image/jpeg", AssetError::Invalid(Rejection::MediaMismatch)),
        (unique_png("charset"), "image/png; charset=binary", AssetError::UnsupportedMediaType),
        (Vec::new(), "image/png", AssetError::InvalidSize),
    ] {
        let sha256 = digest(&bytes);
        assert_eq!(store_asset_bytes(&mut scoped, &store, bytes, media_type, PRODUCER).await, Err(expected), "{media_type}");
        assert_eq!(find_asset(&mut scoped, &sha256).await?, None);
    }
    assert_eq!(store_asset_bytes(&mut scoped, &store, unique_png("producer"), "image/png", "").await, Err(AssetError::InvalidProducerVersion));
    let (registered,): (i64,) = sqlx::query_as("SELECT count(*) FROM objects WHERE tenant_id = $1::uuid AND sha256 = $2")
        .bind(TENANT)
        .bind(digest(&not_an_image).as_slice())
        .fetch_one(scoped.connection())
        .await?;
    assert_eq!(registered, 0);
    assert_eq!(store.get_object(&keys::asset(tenant, &digest(&not_an_image)), 4096).await.ok(), None);
    scoped.rollback().await?;

    for key in stored_keys {
        store.delete_object(&key).await?;
    }
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn documents_attach_only_verified_same_tenant_assets_and_download_only_what_they_attach() -> Result<(), Box<dyn std::error::Error>> {
    let Some((pool, settings)) = probe().await? else {
        return Ok(());
    };
    reset_document_references(&pool).await?;
    let store = ObjectStore::new(&settings)?;
    let client = http_client()?;
    let tenant: TenantId = TENANT.parse()?;
    let member: PrincipalId = MEMBER.parse()?;
    let doc_one: CloudDocumentId = DOC_ONE.parse()?;
    let doc_two: CloudDocumentId = DOC_TWO.parse()?;
    let doc_archived: CloudDocumentId = DOC_ARCHIVED.parse()?;
    let doc_deleting: CloudDocumentId = DOC_DELETING.parse()?;
    let doc_other_tenant: CloudDocumentId = DOC_OTHER_TENANT.parse()?;

    // Two verified assets in tenant one and one in tenant two.
    let (first_bytes, second_bytes, foreign_bytes) = (unique_png("first"), unique_png("second"), unique_png("foreign"));
    let mut setup = scope(&pool, TENANT).await?;
    let first = store_asset_bytes(&mut setup, &store, first_bytes.clone(), "image/png", PRODUCER).await?.object;
    let second = store_asset_bytes(&mut setup, &store, second_bytes, "image/png", PRODUCER).await?.object;
    setup.commit().await?;
    let mut setup = scope(&pool, OTHER_TENANT).await?;
    let foreign = store_asset_bytes(&mut setup, &store, foreign_bytes, "image/png", PRODUCER).await?.object;
    setup.commit().await?;

    // Attach is idempotent and records one `document` reference per document.
    let mut scoped = scope(&pool, TENANT).await?;
    assert_eq!(attach_asset(&mut scoped, doc_one, first.id).await?, Change::Applied);
    assert_eq!(attach_asset(&mut scoped, doc_one, first.id).await?, Change::AlreadyApplied);
    assert_eq!(attach_asset(&mut scoped, doc_one, second.id).await?, Change::Applied);
    assert_eq!(attach_asset(&mut scoped, doc_two, first.id).await?, Change::Applied);
    assert_eq!(reference_count(&mut scoped, first.id).await?.references, 2);
    scoped.commit().await?;

    // Listing is keyset-paged by object id and bounded.
    let mut scoped = scope(&pool, TENANT).await?;
    let mut ids = [first.id, second.id];
    ids.sort();
    let page = list_document_assets(&mut scoped, doc_one, None, 1).await?;
    assert_eq!(page.assets.iter().map(|asset| asset.object_id).collect::<Vec<_>>(), vec![ids[0]]);
    assert_eq!(page.next_after, Some(ids[0]));
    let page = list_document_assets(&mut scoped, doc_one, page.next_after, 1).await?;
    assert_eq!(page.assets.iter().map(|asset| asset.object_id).collect::<Vec<_>>(), vec![ids[1]]);
    assert_eq!(page.next_after, None);
    let all = list_document_assets(&mut scoped, doc_one, None, MAX_ASSET_PAGE).await?;
    assert_eq!(all.assets.len(), 2);
    assert_eq!(all.next_after, None);
    let listed = all.assets.iter().find(|asset| asset.object_id == first.id).ok_or("first asset listed")?;
    assert_eq!((listed.sha256, listed.size_bytes, listed.media_type.as_str()), (first.sha256, first.size_bytes, "image/png"));
    assert!(list_document_assets(&mut scoped, doc_one, Some(ids[1]), 10).await?.assets.is_empty());
    assert!(list_document_assets(&mut scoped, doc_archived, None, 10).await?.assets.is_empty());
    assert_eq!(list_document_assets(&mut scoped, doc_one, None, 0).await, Err(AssetError::InvalidLimit));
    assert_eq!(list_document_assets(&mut scoped, doc_one, None, MAX_ASSET_PAGE + 1).await, Err(AssetError::InvalidLimit));
    assert_eq!(list_document_assets(&mut scoped, CloudDocumentId::random(), None, 10).await, Err(AssetError::DocumentNotFound));
    scoped.rollback().await?;

    // Unverified, rejected, deleting, non-asset and unknown objects are never attached.
    let mut scoped = scope(&pool, TENANT).await?;
    let pending = pending_asset(tenant, &unique_png("pending"));
    register_object(&mut scoped, store.bucket_name(), &pending).await?;
    let rejected = pending_asset(tenant, &unique_png("rejected"));
    register_object(&mut scoped, store.bucket_name(), &rejected).await?;
    assert!(matches!(verify_object(&mut scoped, &store, rejected.id).await?, Some(Verification::Rejected(..))));
    let doomed = pending_asset(tenant, &unique_png("doomed"));
    register_object(&mut scoped, store.bucket_name(), &doomed).await?;
    sqlx::query("UPDATE objects SET state = 'deleting', deleted_at = now() WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(TENANT)
        .bind(doomed.id.to_string())
        .execute(scoped.connection())
        .await?;
    let export_bytes = unique_png("export");
    let export = NewObject {
        id: Uuid::new_v4(),
        kind: ObjectKind::Export,
        object_key: format!("tenants/{tenant}/exports/{}", Uuid::new_v4()),
        ..pending_asset(tenant, &export_bytes)
    };
    register_object(&mut scoped, store.bucket_name(), &export).await?;
    sqlx::query("UPDATE objects SET state = 'verified', verified_at = now() WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(TENANT)
        .bind(export.id.to_string())
        .execute(scoped.connection())
        .await?;
    for id in [pending.id, rejected.id, doomed.id, export.id, Uuid::new_v4(), foreign.id] {
        assert_eq!(attach_asset(&mut scoped, doc_one, id).await, Err(AssetError::AssetUnavailable), "{id}");
    }
    assert_eq!(reference_count(&mut scoped, pending.id).await?.references, 0);

    // Only pending and active documents of this tenant take new attachments.
    assert_eq!(attach_asset(&mut scoped, doc_archived, second.id).await, Err(AssetError::DocumentUnavailable));
    assert_eq!(attach_asset(&mut scoped, doc_deleting, second.id).await, Err(AssetError::DocumentUnavailable));
    assert_eq!(attach_asset(&mut scoped, doc_other_tenant, second.id).await, Err(AssetError::DocumentNotFound));
    assert_eq!(attach_asset(&mut scoped, CloudDocumentId::random(), second.id).await, Err(AssetError::DocumentNotFound));
    // An existing attachment stays idempotent after the document is archived.
    sqlx::query("UPDATE documents SET state = 'archived' WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(TENANT)
        .bind(DOC_TWO)
        .execute(scoped.connection())
        .await?;
    assert_eq!(attach_asset(&mut scoped, doc_two, first.id).await?, Change::AlreadyApplied);
    assert_eq!(attach_asset(&mut scoped, doc_two, second.id).await, Err(AssetError::DocumentUnavailable));
    scoped.rollback().await?;

    // Cross-tenant: tenant two sees neither tenant one's documents nor its objects.
    let mut other = scope(&pool, OTHER_TENANT).await?;
    assert_eq!(attach_asset(&mut other, doc_one, first.id).await, Err(AssetError::DocumentNotFound));
    assert_eq!(attach_asset(&mut other, doc_other_tenant, first.id).await, Err(AssetError::AssetUnavailable));
    assert_eq!(detach_asset(&mut other, doc_one, first.id).await, Err(AssetError::DocumentNotFound));
    assert_eq!(list_document_assets(&mut other, doc_one, None, 10).await, Err(AssetError::DocumentNotFound));
    assert_eq!(
        presign_document_asset_download(&mut other, &store, member, doc_one, first.id, None).await,
        Err(AssetError::NotAttached),
        "tenant one's object does not exist in tenant two"
    );
    assert_eq!(attach_asset(&mut other, doc_other_tenant, foreign.id).await?, Change::Applied);
    other.commit().await?;

    // Document-scoped downloads sign only assets attached to that document.
    let mut scoped = scope(&pool, TENANT).await?;
    let download = presign_document_asset_download(&mut scoped, &store, member, doc_one, first.id, None).await?;
    assert_eq!((download.object_id, download.sha256, download.media_type.as_str()), (first.id, first.sha256, "image/png"));
    assert_eq!(download.expires_in, Duration::from_secs(300));
    let response = client.get(&download.url).send().await?;
    assert!(response.status().is_success());
    assert_eq!(response.bytes().await?.as_ref(), first_bytes.as_slice());
    assert_eq!(presign_document_asset_download(&mut scoped, &store, member, doc_two, second.id, None).await, Err(AssetError::NotAttached));
    assert_eq!(presign_document_asset_download(&mut scoped, &store, member, doc_one, foreign.id, None).await, Err(AssetError::NotAttached));
    assert_eq!(presign_document_asset_download(&mut scoped, &store, member, doc_one, Uuid::new_v4(), None).await, Err(AssetError::NotAttached));
    assert_eq!(
        presign_document_asset_download(&mut scoped, &store, member, CloudDocumentId::random(), first.id, None).await,
        Err(AssetError::DocumentNotFound)
    );
    assert_eq!(
        presign_document_asset_download(&mut scoped, &store, OUTSIDER.parse()?, doc_one, first.id, None).await,
        Err(AssetError::Download(DownloadError::NotAuthorized))
    );
    assert_eq!(
        presign_document_asset_download(&mut scoped, &store, member, doc_one, first.id, Some(Duration::ZERO)).await,
        Err(AssetError::Download(DownloadError::InvalidTtl))
    );
    scoped.commit().await?;

    // An archived document stays readable; a soft-deleted one does not.
    let mut scoped = scope(&pool, TENANT).await?;
    sqlx::query("UPDATE documents SET state = 'archived' WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(TENANT)
        .bind(DOC_TWO)
        .execute(scoped.connection())
        .await?;
    assert!(presign_document_asset_download(&mut scoped, &store, member, doc_two, first.id, None).await.is_ok());
    sqlx::query("UPDATE documents SET state = 'deleting', deleted_at = now() WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(TENANT)
        .bind(DOC_TWO)
        .execute(scoped.connection())
        .await?;
    assert_eq!(presign_document_asset_download(&mut scoped, &store, member, doc_two, first.id, None).await, Err(AssetError::DocumentUnavailable));
    scoped.rollback().await?;

    // Detach is idempotent; a detached asset can no longer be downloaded through the document,
    // and the other document's reference keeps the object alive.
    let mut scoped = scope(&pool, TENANT).await?;
    assert_eq!(detach_asset(&mut scoped, doc_one, first.id).await?, Change::Applied);
    assert_eq!(detach_asset(&mut scoped, doc_one, first.id).await?, Change::AlreadyApplied);
    assert_eq!(detach_asset(&mut scoped, doc_one, Uuid::new_v4()).await?, Change::AlreadyApplied);
    assert_eq!(detach_asset(&mut scoped, CloudDocumentId::random(), first.id).await, Err(AssetError::DocumentNotFound));
    assert_eq!(presign_document_asset_download(&mut scoped, &store, member, doc_one, first.id, None).await, Err(AssetError::NotAttached));
    let remaining = list_document_assets(&mut scoped, doc_one, None, 10).await?;
    assert_eq!(remaining.assets.iter().map(|asset| asset.object_id).collect::<Vec<_>>(), vec![second.id]);
    assert_eq!(reference_count(&mut scoped, first.id).await?.references, 1);
    // Reattaching after a detach works again.
    assert_eq!(attach_asset(&mut scoped, doc_one, first.id).await?, Change::Applied);
    scoped.commit().await?;

    // Exactly one download audit event per issued URL: the doc-one download and the archived one
    // was rolled back, so one.
    let mut audit = scope(&pool, TENANT).await?;
    let (signed,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM audit_events WHERE tenant_id = $1::uuid AND action = $2 AND resource_id = $3::uuid AND actor_principal_id = $4::uuid",
    )
    .bind(TENANT)
    .bind(DOWNLOAD_SIGNED_ACTION)
    .bind(first.id.to_string())
    .bind(MEMBER)
    .fetch_one(audit.connection())
    .await?;
    assert_eq!(signed, 1);
    audit.rollback().await?;

    // Detach everything and remove this run's bytes from the shared bucket.
    let mut scoped = scope(&pool, TENANT).await?;
    for (document, id) in [(doc_one, first.id), (doc_one, second.id), (doc_two, first.id)] {
        assert_eq!(detach_asset(&mut scoped, document, id).await?, Change::Applied);
    }
    assert!(list_document_assets(&mut scoped, doc_one, None, 10).await?.assets.is_empty());
    scoped.commit().await?;
    let mut other = scope(&pool, OTHER_TENANT).await?;
    assert_eq!(detach_asset(&mut other, doc_other_tenant, foreign.id).await?, Change::Applied);
    other.commit().await?;
    for key in [&first.object_key, &second.object_key, &foreign.object_key] {
        store.delete_object(key).await?;
    }
    pool.close().await;
    Ok(())
}

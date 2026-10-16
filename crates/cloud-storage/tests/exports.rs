use std::time::Duration;

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, TenantId, Uuid};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::exports::{
    EXPORT_JOB_KIND, ExportError, ExportFormat, ExportState, MAX_OPEN_EXPORTS_PER_PRINCIPAL, complete_export, expire_exports, fail_export,
    get_export, latest_thumbnail, request_export, start_export, store_thumbnail,
};
use figcraft_cloud_storage::object_metadata::{ObjectKind, ObjectState, keys};
use figcraft_cloud_storage::object_references::Change;
use figcraft_cloud_storage::objects::{ObjectStore, ObjectStoreSettings};
use figcraft_cloud_storage::validation::Rejection;
use sqlx::postgres::{PgPool, PgPoolOptions};

const TENANT: &str = "aa000000-0000-4000-8000-000000000001";
const OTHER_TENANT: &str = "aa000000-0000-4000-8000-000000000002";
const PRINCIPAL: &str = "aa000000-0000-4000-8000-000000000009";
const OTHER_PRINCIPAL: &str = "aa000000-0000-4000-8000-00000000000a";
const PRODUCER: &str = "figcraft-worker test";

type Probe = (PgPool, ObjectStoreSettings);

async fn probe() -> Result<Option<Probe>, Box<dyn std::error::Error>> {
    let (Ok(database_url_file), Ok(s3_directory)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR")) else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; export probe skipped");
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

/// Insert a fresh fixture document (prefix `aa3…`, random tail) with `current_seq`, so every run
/// writes its own thumbnail and export keys.
async fn new_document(pool: &PgPool, tenant: &str, current_seq: i64) -> Result<CloudDocumentId, Box<dyn std::error::Error>> {
    let tail = u64::from_le_bytes(Uuid::new_v4().as_bytes()[..8].try_into()?) & 0xFFFF_FFFF_FFFF;
    let id: CloudDocumentId = format!("aa300000-0000-4000-8000-{tail:012x}").parse()?;
    let n = if tenant == TENANT { "1" } else { "2" };
    sqlx::query(concat!(
        "INSERT INTO documents (id, tenant_id, workspace_id, project_id, name, current_seq) ",
        "VALUES ($1::uuid, $2::uuid, $3::uuid, $4::uuid, 'Export probe document', $5)"
    ))
    .bind(id.to_string())
    .bind(tenant)
    .bind(format!("aa100000-0000-4000-8000-00000000000{n}"))
    .bind(format!("aa200000-0000-4000-8000-00000000000{n}"))
    .bind(current_seq)
    .execute(pool)
    .await?;
    Ok(id)
}

/// A valid 1x1 PNG unique to this run (a `tEXt` chunk carries a random id).
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
    chunk(&mut png, b"tEXt", format!("Comment\0figcraft export probe {label} {}", Uuid::new_v4()).as_bytes());
    chunk(&mut png, b"IDAT", &[0x78, 0x01, 0x01, 0x02, 0x00, 0xfd, 0xff, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01]);
    chunk(&mut png, b"IEND", &[]);
    png
}

/// A lossless WebP header of `width`×`height` followed by random payload bytes, so each call is
/// distinct content. The validator reads only the header.
fn unique_webp(width: u32, height: u32) -> Vec<u8> {
    let bits = (width - 1) | ((height - 1) << 14);
    let mut data = vec![0x2F];
    data.extend(bits.to_le_bytes());
    data.extend(Uuid::new_v4().as_bytes());
    let mut chunk = b"VP8L".to_vec();
    chunk.extend(u32::try_from(data.len()).unwrap_or(u32::MAX).to_le_bytes());
    chunk.extend(&data);
    if data.len() % 2 == 1 {
        chunk.push(0);
    }
    let mut out = b"RIFF".to_vec();
    out.extend(u32::try_from(chunk.len() + 4).unwrap_or(u32::MAX).to_le_bytes());
    out.extend(b"WEBP");
    out.extend(chunk);
    out
}

async fn job_row(pool: &PgPool, id: Uuid) -> Result<(String, String, i32, Option<String>, Option<String>, String), Box<dyn std::error::Error>> {
    Ok(sqlx::query_as("SELECT kind, state, attempts, error_code, result::text, input::text FROM jobs WHERE id = $1::uuid")
        .bind(id.to_string())
        .fetch_one(pool)
        .await?)
}

async fn reference_owners(pool: &PgPool, object: Uuid) -> Result<Vec<(String, String)>, Box<dyn std::error::Error>> {
    Ok(sqlx::query_as("SELECT owner_kind, owner_id::text FROM object_references WHERE object_id = $1::uuid ORDER BY owner_kind")
        .bind(object.to_string())
        .fetch_all(pool)
        .await?)
}

/// The SQLSTATE of a direct update, proving the table refuses a state the code never writes.
async fn sqlstate(pool: &PgPool, sql: &'static str, id: Uuid) -> Option<String> {
    sqlx::query(sql)
        .bind(id.to_string())
        .execute(pool)
        .await
        .err()
        .and_then(|error| error.as_database_error().and_then(|db| db.code()).map(String::from))
}

macro_rules! scope {
    ($pool:expr, $tenant:expr) => {
        begin_tenant_transaction($pool, $tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?
    };
}

#[tokio::test]
async fn exports_move_through_a_constraint_backed_state_machine_and_store_verified_results() -> Result<(), Box<dyn std::error::Error>> {
    let Some((pool, settings)) = probe().await? else {
        return Ok(());
    };
    let store = ObjectStore::new(&settings)?;
    let tenant: TenantId = TENANT.parse()?;
    let other: TenantId = OTHER_TENANT.parse()?;
    let principal: PrincipalId = PRINCIPAL.parse()?;
    let outsider: PrincipalId = OTHER_PRINCIPAL.parse()?;
    let document = new_document(&pool, TENANT, 5).await?;
    let other_document = new_document(&pool, OTHER_TENANT, 5).await?;
    let options = serde_json::json!({ "scale": 2, "nodes": ["1:2"] });
    let mut written = Vec::new();

    // Refusals before anything is written.
    let mut refused = scope!(&pool, tenant);
    for (document, principal, options, seq, expected) in [
        (document, principal, options.clone(), 6, ExportError::SeqAhead),
        (document, principal, options.clone(), u64::MAX, ExportError::InvalidSeq),
        (document, principal, serde_json::json!([1]), 0, ExportError::InvalidOptions),
        (document, outsider, options.clone(), 0, ExportError::NotAuthorized),
        (document, PrincipalId::random(), options.clone(), 0, ExportError::NotAuthorized),
        (CloudDocumentId::random(), principal, options.clone(), 0, ExportError::NotFound),
        // Another tenant's document is invisible from this tenant.
        (other_document, principal, options.clone(), 0, ExportError::NotFound),
    ] {
        assert_eq!(request_export(&mut refused, document, principal, ExportFormat::Png, &options, seq).await, Err(expected));
    }
    refused.rollback().await?;

    // A request records a queued export and its queued job.
    let mut scoped = scope!(&pool, tenant);
    let requested = request_export(&mut scoped, document, principal, ExportFormat::Png, &options, 5).await?;
    scoped.commit().await?;
    assert_eq!((requested.state, requested.format, requested.requested_seq), (ExportState::Queued, ExportFormat::Png, 5));
    assert_eq!((requested.tenant_id, requested.document_id, requested.requested_by), (tenant, document, principal));
    assert_eq!((requested.options.clone(), requested.result_object_id, requested.error_code.clone()), (options.clone(), None, None));
    assert_eq!(requested.completed_at_unix, None);
    let job = requested.job_id.ok_or("export without job")?;
    let (kind, state, attempts, _, _, input) = job_row(&pool, job).await?;
    assert_eq!((kind.as_str(), state.as_str(), attempts), (EXPORT_JOB_KIND, "queued", 0));
    let input: serde_json::Value = serde_json::from_str(&input)?;
    assert_eq!(input["export_id"], requested.id.to_string());
    assert_eq!(input["document_id"], document.to_string());

    // The table backs the state machine: no result without success, no success without a result,
    // no failure without a code, no completion time on an open export.
    for sql in [
        "UPDATE exports SET state = 'succeeded', completed_at = now() WHERE id = $1::uuid",
        "UPDATE exports SET state = 'failed', completed_at = now() WHERE id = $1::uuid",
        "UPDATE exports SET completed_at = now() WHERE id = $1::uuid",
        "UPDATE exports SET error_code = 'x' WHERE id = $1::uuid",
        "UPDATE exports SET state = 'done' WHERE id = $1::uuid",
        "UPDATE exports SET options = '[]'::jsonb WHERE id = $1::uuid",
        "UPDATE exports SET format = 'gif' WHERE id = $1::uuid",
    ] {
        assert_eq!(sqlstate(&pool, sql, requested.id).await.as_deref(), Some("23514"), "{sql}");
    }

    // A queued export cannot complete; a bad code cannot fail it.
    let mut early = scope!(&pool, tenant);
    assert_eq!(complete_export(&mut early, &store, requested.id, unique_png("early"), "a.png", PRODUCER).await, Err(ExportError::InvalidState));
    assert_eq!(fail_export(&mut early, requested.id, "Render Failed").await, Err(ExportError::InvalidErrorCode));
    early.rollback().await?;

    // Start moves export and job to running; starting again resumes without a second attempt.
    let mut start = scope!(&pool, tenant);
    let running = start_export(&mut start, requested.id).await?;
    assert_eq!((running.state, running.completed_at_unix), (ExportState::Running, None));
    assert_eq!(start_export(&mut start, requested.id).await?, running);
    start.commit().await?;
    let (_, state, attempts, ..) = job_row(&pool, job).await?;
    assert_eq!((state.as_str(), attempts), ("running", 1));

    // Content is validated by format, and a refusal changes nothing.
    let png = unique_png("result");
    let mut invalid = scope!(&pool, tenant);
    for (bytes, expected) in [
        (b"<svg/>".to_vec(), ExportError::InvalidContent(Rejection::MediaMismatch)),
        (png[..20].to_vec(), ExportError::InvalidContent(Rejection::Truncated)),
        (Vec::new(), ExportError::InvalidSize),
    ] {
        assert_eq!(complete_export(&mut invalid, &store, requested.id, bytes, "a.png", PRODUCER).await, Err(expected));
    }
    assert_eq!(complete_export(&mut invalid, &store, requested.id, png.clone(), "a.png", "").await, Err(ExportError::InvalidProducerVersion));
    assert_eq!(get_export(&mut invalid, requested.id).await?.map(|export| export.state), Some(ExportState::Running));
    invalid.rollback().await?;

    // Completion stores the bytes under the export's sanitized key, verified and referenced.
    let mut complete = scope!(&pool, tenant);
    let completed = complete_export(&mut complete, &store, requested.id, png.clone(), "../../Users/Me/Frame 1.PNG", PRODUCER).await?;
    complete.commit().await?;
    let object = &completed.object;
    written.push(object.object_key.clone());
    assert_eq!(object.object_key, format!("tenants/{tenant}/exports/{}/frame-1.png", requested.id));
    assert_eq!((object.kind, object.state, object.media_type.as_str()), (ObjectKind::Export, ObjectState::Verified, "image/png"));
    assert_eq!(object.size_bytes, i64::try_from(png.len())?);
    assert_eq!(store.get_object(&object.object_key, 1 << 20).await?, png);
    assert_eq!((completed.export.state, completed.export.result_object_id), (ExportState::Succeeded, Some(object.id)));
    assert!(completed.export.completed_at_unix.is_some());
    assert_eq!(reference_owners(&pool, object.id).await?, vec![("export".to_string(), requested.id.to_string())]);
    let (_, state, _, _, result, _) = job_row(&pool, job).await?;
    assert_eq!(state, "succeeded");
    let result: serde_json::Value = serde_json::from_str(&result.ok_or("job without result")?)?;
    assert_eq!(result["object_id"], object.id.to_string());
    // The table refuses a success that loses its result.
    assert_eq!(sqlstate(&pool, "UPDATE exports SET result_object_id = NULL WHERE id = $1::uuid", requested.id).await.as_deref(), Some("23514"));

    // A finished export accepts no further transition.
    let mut done = scope!(&pool, tenant);
    assert_eq!(complete_export(&mut done, &store, requested.id, unique_png("again"), "b.png", PRODUCER).await, Err(ExportError::InvalidState));
    assert_eq!(start_export(&mut done, requested.id).await, Err(ExportError::InvalidState));
    assert_eq!(fail_export(&mut done, requested.id, "late").await, Err(ExportError::InvalidState));
    done.rollback().await?;

    // A queued export can fail with a fixed code; its job fails with it.
    let mut failing = scope!(&pool, tenant);
    let doomed = request_export(&mut failing, document, principal, ExportFormat::Pdf, &serde_json::json!({}), 0).await?;
    let failed = fail_export(&mut failing, doomed.id, "render_failed").await?;
    failing.commit().await?;
    assert_eq!((failed.state, failed.error_code.as_deref(), failed.result_object_id), (ExportState::Failed, Some("render_failed"), None));
    assert!(failed.completed_at_unix.is_some());
    let (_, state, _, error_code, ..) = job_row(&pool, doomed.job_id.ok_or("no job")?).await?;
    assert_eq!((state.as_str(), error_code.as_deref()), ("failed", Some("render_failed")));

    // SVG and PDF results get their structural checks; active content is refused.
    let mut vector = scope!(&pool, tenant);
    let svg = request_export(&mut vector, document, principal, ExportFormat::Svg, &serde_json::json!({}), 3).await?;
    start_export(&mut vector, svg.id).await?;
    let script = format!("<svg xmlns=\"http://www.w3.org/2000/svg\"><script>alert(1)</script><!-- {} --></svg>", Uuid::new_v4());
    assert_eq!(complete_export(&mut vector, &store, svg.id, script.into_bytes(), "x.svg", PRODUCER).await, Err(ExportError::ActiveContent));
    let body = format!("<svg width=\"1\" height=\"1\" xmlns=\"http://www.w3.org/2000/svg\"><!-- {} --></svg>\n", Uuid::new_v4());
    let svg_done = complete_export(&mut vector, &store, svg.id, body.clone().into_bytes(), "Logo", PRODUCER).await?;
    written.push(svg_done.object.object_key.clone());
    assert!(svg_done.object.object_key.ends_with("/logo.svg"), "{}", svg_done.object.object_key);
    assert_eq!(svg_done.object.media_type, "image/svg+xml");
    let pdf = request_export(&mut vector, document, principal, ExportFormat::Pdf, &serde_json::json!({}), 3).await?;
    start_export(&mut vector, pdf.id).await?;
    let action = format!("%PDF-1.7\n<< /OpenAction << /S /JavaScript /JS (app.alert(1)) >> >>\n% {}\n%%EOF\n", Uuid::new_v4());
    assert_eq!(complete_export(&mut vector, &store, pdf.id, action.into_bytes(), "x.pdf", PRODUCER).await, Err(ExportError::ActiveContent));
    let body = format!("%PDF-1.7\n1 0 obj << /Type /Catalog >> endobj\n% {}\n%%EOF\n", Uuid::new_v4());
    let pdf_done = complete_export(&mut vector, &store, pdf.id, body.into_bytes(), "Board.pdf", PRODUCER).await?;
    written.push(pdf_done.object.object_key.clone());
    assert_eq!(pdf_done.object.media_type, "application/pdf");
    // `.figcraft` results are validated as zip containers.
    let archive = request_export(&mut vector, document, principal, ExportFormat::Figcraft, &serde_json::json!({}), 3).await?;
    start_export(&mut vector, archive.id).await?;
    let junk = complete_export(&mut vector, &store, archive.id, b"PK\x03\x04junk".to_vec(), "doc", PRODUCER).await;
    assert!(matches!(junk, Err(ExportError::InvalidContent(_))), "{junk:?}");
    // The worker records the refusal, so the export does not stay open.
    assert_eq!(fail_export(&mut vector, archive.id, "invalid_content").await?.state, ExportState::Failed);
    vector.commit().await?;

    // Another tenant sees none of it and cannot drive it.
    let mut foreign = scope!(&pool, other);
    assert_eq!(get_export(&mut foreign, requested.id).await?, None);
    assert_eq!(start_export(&mut foreign, svg.id).await, Err(ExportError::NotFound));
    assert_eq!(fail_export(&mut foreign, archive.id, "stolen").await, Err(ExportError::NotFound));
    assert_eq!(complete_export(&mut foreign, &store, archive.id, unique_png("x"), "x", PRODUCER).await, Err(ExportError::NotFound));
    assert_eq!(request_export(&mut foreign, document, outsider, ExportFormat::Png, &serde_json::json!({}), 0).await, Err(ExportError::NotFound));
    assert_eq!(expire_exports(&mut foreign, Duration::ZERO, 100).await?, Vec::new());
    foreign.rollback().await?;

    // Expiry retires only old succeeded exports and drops their references; the object stays for
    // the collector.
    let mut sweep = scope!(&pool, tenant);
    assert!(expire_exports(&mut sweep, Duration::ZERO, 500).await?.iter().all(|expired| expired.export_id != requested.id));
    assert_eq!(expire_exports(&mut sweep, Duration::ZERO, 0).await?, Vec::new());
    sweep.rollback().await?;
    sqlx::query("UPDATE exports SET completed_at = completed_at - interval '2 days' WHERE id = $1::uuid")
        .bind(requested.id.to_string())
        .execute(&pool)
        .await?;
    let mut sweep = scope!(&pool, tenant);
    let expired = expire_exports(&mut sweep, Duration::from_secs(24 * 60 * 60), i64::MAX).await?;
    sweep.commit().await?;
    assert!(expired.iter().any(|expired| expired.export_id == requested.id && expired.object_id == object.id), "{expired:?}");
    assert!(expired.iter().all(|expired| expired.export_id != svg.id && expired.export_id != doomed.id));
    let mut check = scope!(&pool, tenant);
    let retired = get_export(&mut check, requested.id).await?.ok_or("export vanished")?;
    assert_eq!((retired.state, retired.result_object_id), (ExportState::Expired, None));
    assert!(retired.completed_at_unix.is_some());
    assert_eq!(start_export(&mut check, requested.id).await, Err(ExportError::InvalidState));
    check.rollback().await?;
    assert_eq!(reference_owners(&pool, object.id).await?, Vec::new());
    assert_eq!(reference_owners(&pool, svg_done.object.id).await?.len(), 1);

    // Open exports per principal are bounded.
    let mut flood = scope!(&pool, tenant);
    let mut made = 0;
    let outcome = loop {
        match request_export(&mut flood, document, principal, ExportFormat::Webp, &serde_json::json!({}), 0).await {
            Ok(_) if made <= MAX_OPEN_EXPORTS_PER_PRINCIPAL => made += 1,
            other => break other.map(|_| ()),
        }
    };
    assert_eq!(outcome, Err(ExportError::TooManyOpenExports));
    assert!(made <= MAX_OPEN_EXPORTS_PER_PRINCIPAL);
    flood.rollback().await?;

    for key in &written {
        store.delete_object(key).await?;
    }
    Ok(())
}

#[tokio::test]
async fn thumbnails_are_validated_webp_one_per_sequence_and_newer_ones_replace_older() -> Result<(), Box<dyn std::error::Error>> {
    let Some((pool, settings)) = probe().await? else {
        return Ok(());
    };
    let store = ObjectStore::new(&settings)?;
    let tenant: TenantId = TENANT.parse()?;
    let other: TenantId = OTHER_TENANT.parse()?;
    let document = new_document(&pool, TENANT, 5).await?;
    let mut written = Vec::new();

    let mut refused = scope!(&pool, tenant);
    assert_eq!(latest_thumbnail(&mut refused, document).await?, None);
    for (seq, bytes, expected) in [
        (6, unique_webp(64, 64), ExportError::SeqAhead),
        (u64::MAX, unique_webp(64, 64), ExportError::InvalidSeq),
        (1, unique_png("thumb"), ExportError::InvalidContent(Rejection::MediaMismatch)),
        (1, unique_webp(2000, 10), ExportError::InvalidContent(Rejection::DimensionTooLarge)),
        (1, Vec::new(), ExportError::InvalidSize),
        (1, vec![0; 2 * 1024 * 1024], ExportError::InvalidSize),
    ] {
        assert_eq!(store_thumbnail(&mut refused, &store, document, seq, bytes, PRODUCER).await, Err(expected), "{seq}");
    }
    assert_eq!(store_thumbnail(&mut refused, &store, CloudDocumentId::random(), 0, unique_webp(8, 8), PRODUCER).await, Err(ExportError::NotFound));
    refused.rollback().await?;

    // The first thumbnail of a sequence is stored, verified and referenced by the document.
    let first = unique_webp(320, 200);
    let mut scoped = scope!(&pool, tenant);
    let stored = store_thumbnail(&mut scoped, &store, document, 2, first.clone(), PRODUCER).await?;
    scoped.commit().await?;
    let object = stored.thumbnail.object.clone();
    written.push(object.object_key.clone());
    assert_eq!((stored.change, stored.superseded.clone(), stored.thumbnail.seq), (Change::Applied, Vec::new(), 2));
    assert_eq!(object.object_key, keys::thumbnail(tenant, document, 2));
    assert_eq!((object.kind, object.state, object.media_type.as_str()), (ObjectKind::Thumbnail, ObjectState::Verified, "image/webp"));
    assert_eq!(store.get_object(&object.object_key, 1 << 20).await?, first);
    assert_eq!(reference_owners(&pool, object.id).await?, vec![("thumbnail".to_string(), document.to_string())]);

    // Identical bytes again are a no-op; different bytes for a stored sequence are refused.
    let mut again = scope!(&pool, tenant);
    let repeat = store_thumbnail(&mut again, &store, document, 2, first.clone(), PRODUCER).await?;
    assert_eq!((repeat.change, repeat.thumbnail.object.id), (Change::AlreadyApplied, object.id));
    assert_eq!(store_thumbnail(&mut again, &store, document, 2, unique_webp(320, 200), PRODUCER).await, Err(ExportError::ThumbnailExists));
    again.rollback().await?;
    assert_eq!(store.get_object(&object.object_key, 1 << 20).await?, first);

    // A newer sequence replaces the older thumbnail; an older one is then superseded.
    let mut newer = scope!(&pool, tenant);
    let replaced = store_thumbnail(&mut newer, &store, document, 4, unique_webp(320, 200), PRODUCER).await?;
    newer.commit().await?;
    written.push(replaced.thumbnail.object.object_key.clone());
    assert_eq!((replaced.change, replaced.superseded.clone()), (Change::Applied, vec![object.id]));
    assert_eq!(reference_owners(&pool, object.id).await?, Vec::new());
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM document_thumbnails WHERE document_id = $1::uuid")
        .bind(document.to_string())
        .fetch_one(&pool)
        .await?;
    assert_eq!(rows, 1);
    let mut late = scope!(&pool, tenant);
    assert_eq!(store_thumbnail(&mut late, &store, document, 3, unique_webp(8, 8), PRODUCER).await, Err(ExportError::Superseded));
    assert_eq!(store_thumbnail(&mut late, &store, document, 2, first.clone(), PRODUCER).await, Err(ExportError::Superseded));
    let latest = latest_thumbnail(&mut late, document).await?.ok_or("no latest thumbnail")?;
    assert_eq!((latest.seq, latest.object.id), (4, replaced.thumbnail.object.id));
    late.rollback().await?;

    // Another tenant cannot see or write the document's thumbnails.
    let mut foreign = scope!(&pool, other);
    assert_eq!(latest_thumbnail(&mut foreign, document).await?, None);
    assert_eq!(store_thumbnail(&mut foreign, &store, document, 5, unique_webp(8, 8), PRODUCER).await, Err(ExportError::NotFound));
    foreign.rollback().await?;
    // The table refuses a thumbnail row pointing at another tenant's object.
    let crossed = sqlx::query("INSERT INTO document_thumbnails (tenant_id, document_id, seq, object_id) VALUES ($1::uuid, $2::uuid, 5, $3::uuid)")
        .bind(OTHER_TENANT)
        .bind(document.to_string())
        .bind(replaced.thumbnail.object.id.to_string())
        .execute(&pool)
        .await;
    assert_eq!(crossed.err().and_then(|error| error.as_database_error().and_then(|db| db.code()).map(String::from)).as_deref(), Some("23503"));

    for key in &written {
        store.delete_object(key).await?;
    }
    Ok(())
}

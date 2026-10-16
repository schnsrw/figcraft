use figcraft_cloud_domain::{TenantId, Uuid};
use figcraft_cloud_storage::object_metadata::{NewObject, ObjectKind, ObjectState, Verification, keys, register_object, verify_object};
use figcraft_cloud_storage::object_references::{Change, OwnerKind, ReferenceOwner, list_references, reference_count};
use figcraft_cloud_storage::objects::{ObjectStore, ObjectStoreError, ObjectStoreSettings};
use figcraft_cloud_storage::plugin_store::{
    MAX_VERSION_PAGE, ManifestProblem, PluginName, PluginState, PluginStoreError, PluginVersionState, find_plugin, get_manifest, get_plugin,
    install_plugin_version, list_versions, withdraw_plugin_version,
};
use figcraft_cloud_storage::validation::{MEDIA_PLUGIN, Rejection};
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgPool, PgPoolOptions};

const TENANT: &str = "a9000000-0000-4000-8000-000000000001";
const OTHER_TENANT: &str = "a9000000-0000-4000-8000-000000000002";
const PRODUCER: &str = "figcraft-cloud test";
const WASM: &[u8] = b"\0asm\x01\0\0\0";

type Probe = (PgPool, ObjectStoreSettings);

async fn probe() -> Result<Option<Probe>, Box<dyn std::error::Error>> {
    let (Ok(database_url_file), Ok(s3_directory)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR")) else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; plugin store probe skipped");
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

async fn begin(pool: &PgPool, tenant: TenantId) -> Result<TenantTransaction<'_>, Box<dyn std::error::Error>> {
    begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}").into())
}

/// A plugin bundle in the `FCPLUGB1` layout.
fn bundle(manifest: &str, assets: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = b"FCPLUGB1".to_vec();
    for part in [manifest.as_bytes(), WASM] {
        out.extend(u32::try_from(part.len()).unwrap_or(u32::MAX).to_le_bytes());
        out.extend(part);
    }
    out.extend(u32::try_from(assets.len()).unwrap_or(u32::MAX).to_le_bytes());
    for (name, data) in assets {
        for part in [name.as_bytes(), *data] {
            out.extend(u32::try_from(part.len()).unwrap_or(u32::MAX).to_le_bytes());
            out.extend(part);
        }
    }
    out
}

/// A manifest unique to this run (the description carries `nonce`), so reruns never collide on
/// content-addressed keys.
fn manifest(id: &str, version: Option<&str>, nonce: &str) -> String {
    let version = version.map(|version| format!(r#","version":"{version}""#)).unwrap_or_default();
    format!(r#"{{"id":"{id}","name":"Probe","main":"probe.wasm","api":1,"description":"plugin store probe {nonce}"{version}}}"#)
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

#[tokio::test]
async fn bundles_install_once_per_version_with_verified_bytes_manifest_and_reference() -> Result<(), Box<dyn std::error::Error>> {
    let Some((pool, settings)) = probe().await? else {
        return Ok(());
    };
    let store = ObjectStore::new(&settings)?;
    let tenant: TenantId = TENANT.parse()?;
    let nonce = Uuid::new_v4().to_string();
    let plugin_name = format!("Probe {nonce}");
    let name = PluginName { publisher: "Acme Probes", name: &plugin_name };
    let first = bundle(&manifest("org.example.probe", Some("1.0.0"), &nonce), &[("icon.png", b"123"), ("LICENSE.txt", b"MIT")]);
    let mut written = Vec::new();

    // First install creates the plugin, stores and verifies the bytes at the content-addressed key,
    // records the manifest and references the object from the version.
    let mut scoped = begin(&pool, tenant).await?;
    assert_eq!(find_plugin(&mut scoped, name).await?, None);
    let installed = install_plugin_version(&mut scoped, &store, name, "1.0.0", &first, PRODUCER).await?;
    scoped.commit().await?;
    assert!(!installed.reused);
    assert_eq!((installed.plugin.publisher.as_str(), installed.plugin.name.as_str()), ("Acme Probes", plugin_name.as_str()));
    assert_eq!(installed.plugin.state, PluginState::Active);
    assert_eq!((installed.version.version.as_str(), installed.version.state), ("1.0.0", PluginVersionState::Verified));
    assert_eq!((installed.version.plugin_id, installed.version.object_id), (installed.plugin.id, installed.object.id));
    assert_eq!(installed.version.sha256, digest(&first));
    let key = keys::plugin(tenant, installed.plugin.id, "1.0.0", &digest(&first)).ok_or("key")?;
    assert_eq!(installed.object.object_key, key);
    assert_eq!((installed.object.kind, installed.object.state), (ObjectKind::Plugin, ObjectState::Verified));
    assert_eq!((installed.object.media_type.as_str(), installed.object.size_bytes), (MEDIA_PLUGIN, i64::try_from(first.len())?));
    assert_eq!(store.get_object(&key, 1 << 20).await?, first);
    written.push(key.clone());

    let mut read = begin(&pool, tenant).await?;
    assert_eq!(find_plugin(&mut read, name).await?, Some(installed.plugin.clone()));
    assert_eq!(get_plugin(&mut read, installed.plugin.id).await?, Some(installed.plugin.clone()));
    assert_eq!(
        list_references(&mut read, installed.object.id, 10).await?,
        vec![ReferenceOwner::Owner(OwnerKind::PluginVersion, installed.version.id)]
    );
    let stored = get_manifest(&mut read, installed.plugin.id, "1.0.0").await?.ok_or("manifest")?;
    assert_eq!(stored.version, installed.version);
    assert_eq!(stored.manifest["id"], "org.example.probe");
    assert_eq!(stored.manifest["description"], format!("plugin store probe {nonce}"));
    assert_eq!(get_manifest(&mut read, installed.plugin.id, "9.9.9").await?, None);
    read.rollback().await?;

    // Re-installing the identical bundle is idempotent; a different bundle for the version is not.
    let mut again = begin(&pool, tenant).await?;
    let reused = install_plugin_version(&mut again, &store, name, "1.0.0", &first, PRODUCER).await?;
    assert!(reused.reused);
    assert_eq!((reused.plugin.id, reused.version.id, reused.object.id), (installed.plugin.id, installed.version.id, installed.object.id));
    let different = bundle(&manifest("org.example.probe", Some("1.0.0"), &format!("{nonce} changed")), &[]);
    assert_eq!(install_plugin_version(&mut again, &store, name, "1.0.0", &different, PRODUCER).await, Err(PluginStoreError::Conflict));
    again.rollback().await?;
    assert_eq!(
        store.get_object(&keys::plugin(tenant, installed.plugin.id, "1.0.0", &digest(&different)).ok_or("key")?, 1 << 20).await,
        Err(ObjectStoreError::NotFound)
    );

    // A later version needs the same manifest id, and a manifest version must match the request.
    let second = bundle(&manifest("org.example.probe", None, &nonce), &[]);
    let mut next = begin(&pool, tenant).await?;
    let renamed = bundle(&manifest("org.example.other", Some("1.1.0"), &nonce), &[]);
    assert_eq!(install_plugin_version(&mut next, &store, name, "1.1.0", &renamed, PRODUCER).await, Err(PluginStoreError::IdMismatch));
    next.rollback().await?;
    let mut next = begin(&pool, tenant).await?;
    let mislabelled = bundle(&manifest("org.example.probe", Some("1.2.0"), &nonce), &[]);
    assert_eq!(install_plugin_version(&mut next, &store, name, "1.1.0", &mislabelled, PRODUCER).await, Err(PluginStoreError::VersionMismatch));
    let later = install_plugin_version(&mut next, &store, name, "1.1.0", &second, PRODUCER).await?;
    next.commit().await?;
    assert!(!later.reused);
    assert_eq!(later.plugin.id, installed.plugin.id);
    written.push(later.object.object_key.clone());

    // Listing is oldest first and bounded.
    let mut list = begin(&pool, tenant).await?;
    let versions = list_versions(&mut list, installed.plugin.id, false, 10).await?;
    assert_eq!(versions.iter().map(|version| version.version.as_str()).collect::<Vec<_>>(), ["1.0.0", "1.1.0"]);
    assert_eq!(list_versions(&mut list, installed.plugin.id, false, 1).await?.len(), 1);
    for limit in [0, MAX_VERSION_PAGE + 1] {
        assert_eq!(list_versions(&mut list, installed.plugin.id, false, limit).await, Err(PluginStoreError::InvalidLimit));
    }
    assert_eq!(list_versions(&mut list, Uuid::new_v4(), true, 10).await?, Vec::new());
    list.rollback().await?;

    // Withdrawal hides a version from listings and re-installs but keeps the row, manifest,
    // reference and bytes.
    let mut withdraw = begin(&pool, tenant).await?;
    assert_eq!(withdraw_plugin_version(&mut withdraw, installed.plugin.id, "1.0.0").await?, Change::Applied);
    assert_eq!(withdraw_plugin_version(&mut withdraw, installed.plugin.id, "1.0.0").await?, Change::AlreadyApplied);
    assert_eq!(withdraw_plugin_version(&mut withdraw, installed.plugin.id, "7.0.0").await, Err(PluginStoreError::NotFound));
    assert_eq!(withdraw_plugin_version(&mut withdraw, Uuid::new_v4(), "1.0.0").await, Err(PluginStoreError::NotFound));
    assert_eq!(withdraw_plugin_version(&mut withdraw, installed.plugin.id, "../1").await, Err(PluginStoreError::InvalidVersion));
    withdraw.commit().await?;
    let mut after = begin(&pool, tenant).await?;
    let visible = list_versions(&mut after, installed.plugin.id, false, 10).await?;
    assert_eq!(visible.iter().map(|version| version.version.as_str()).collect::<Vec<_>>(), ["1.1.0"]);
    let all = list_versions(&mut after, installed.plugin.id, true, 10).await?;
    assert_eq!(
        all.iter().map(|version| (version.version.as_str(), version.state)).collect::<Vec<_>>(),
        [("1.0.0", PluginVersionState::Withdrawn), ("1.1.0", PluginVersionState::Verified)]
    );
    let withdrawn = get_manifest(&mut after, installed.plugin.id, "1.0.0").await?.ok_or("manifest")?;
    assert_eq!(withdrawn.version.state, PluginVersionState::Withdrawn);
    assert_eq!(reference_count(&mut after, installed.object.id).await?.references, 1);
    assert_eq!(install_plugin_version(&mut after, &store, name, "1.0.0", &first, PRODUCER).await, Err(PluginStoreError::InvalidState));
    after.rollback().await?;
    assert_eq!(store.get_object(&key, 1 << 20).await?, first);

    // Another tenant sees none of it; the same name there is a different plugin.
    let other: TenantId = OTHER_TENANT.parse()?;
    let mut foreign = begin(&pool, other).await?;
    assert_eq!(get_plugin(&mut foreign, installed.plugin.id).await?, None);
    assert_eq!(find_plugin(&mut foreign, name).await?, None);
    assert_eq!(list_versions(&mut foreign, installed.plugin.id, true, 10).await?, Vec::new());
    assert_eq!(get_manifest(&mut foreign, installed.plugin.id, "1.1.0").await?, None);
    assert_eq!(withdraw_plugin_version(&mut foreign, installed.plugin.id, "1.1.0").await, Err(PluginStoreError::NotFound));
    let theirs = install_plugin_version(&mut foreign, &store, name, "1.1.0", &second, PRODUCER).await?;
    assert_ne!(theirs.plugin.id, installed.plugin.id);
    assert!(theirs.object.object_key.starts_with(&format!("tenants/{other}/plugins/")));
    let their_key = theirs.object.object_key.clone();
    foreign.rollback().await?;
    written.push(their_key);

    // A disabled plugin takes no new versions.
    let mut disable = begin(&pool, tenant).await?;
    sqlx::query("UPDATE plugins SET state = 'disabled' WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(tenant.to_string())
        .bind(installed.plugin.id.to_string())
        .execute(disable.connection())
        .await?;
    let third = bundle(&manifest("org.example.probe", Some("2.0.0"), &nonce), &[]);
    assert_eq!(install_plugin_version(&mut disable, &store, name, "2.0.0", &third, PRODUCER).await, Err(PluginStoreError::InvalidState));
    disable.rollback().await?;

    for key in written {
        store.delete_object(&key).await?;
    }
    Ok(())
}

#[tokio::test]
async fn junk_bundles_names_and_blocked_keys_are_refused_without_writing() -> Result<(), Box<dyn std::error::Error>> {
    let Some((pool, settings)) = probe().await? else {
        return Ok(());
    };
    let store = ObjectStore::new(&settings)?;
    let tenant: TenantId = TENANT.parse()?;
    let nonce = Uuid::new_v4().to_string();
    let plugin_name = format!("Junk {nonce}");
    let name = PluginName { publisher: "Acme Probes", name: &plugin_name };
    let good = bundle(&manifest("org.example.junk", Some("1.0.0"), &nonce), &[]);

    // Everything about the request and the bytes is checked before any SQL runs, so one
    // transaction survives every refusal and no plugin row appears.
    let mut scoped = begin(&pool, tenant).await?;
    let mut trailing = good.clone();
    trailing.push(0);
    let mut lying_length = good.clone();
    lying_length[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
    let mut short_length = good.clone();
    short_length[8..12].copy_from_slice(&200_000u32.to_le_bytes());
    let numbers = r#"{"id":"a","name":"A","main":"a","x":[1e400]}"#;
    for (bytes, error) in [
        (Vec::new(), PluginStoreError::InvalidBundle(Rejection::Empty)),
        (b"not a bundle".to_vec(), PluginStoreError::InvalidBundle(Rejection::MediaMismatch)),
        (b"FCPLUGB1".to_vec(), PluginStoreError::InvalidBundle(Rejection::Truncated)),
        (trailing, PluginStoreError::InvalidBundle(Rejection::TrailingBytes)),
        (lying_length, PluginStoreError::InvalidBundle(Rejection::EntrySizeBudget)),
        (short_length, PluginStoreError::InvalidBundle(Rejection::Truncated)),
        (bundle(&manifest("org.example.junk", Some("1.0.0"), &nonce), &[("../x", b"1")]), PluginStoreError::InvalidBundle(Rejection::UnsafeName)),
        (bundle("[]", &[]), PluginStoreError::InvalidManifest(ManifestProblem::NotObject)),
        (bundle("{", &[]), PluginStoreError::InvalidManifest(ManifestProblem::NotJson)),
        (bundle(r#"{"id":"a","name":"A","main":"a","x":"\u0000"}"#, &[]), PluginStoreError::InvalidManifest(ManifestProblem::NulCharacter)),
        (bundle(r#"{"id":"a/b","name":"A","main":"a"}"#, &[]), PluginStoreError::InvalidManifest(ManifestProblem::InvalidId)),
        (bundle(&manifest("org.example.junk", Some("3.0.0"), &nonce), &[]), PluginStoreError::VersionMismatch),
        // Numbers past f64 range are not JSON this parser accepts.
        (bundle(numbers, &[]), PluginStoreError::InvalidManifest(ManifestProblem::NotJson)),
    ] {
        assert_eq!(install_plugin_version(&mut scoped, &store, name, "1.0.0", &bytes, PRODUCER).await, Err(error));
    }
    for (publisher, plugin, version, producer, error) in [
        ("", "P", "1.0.0", PRODUCER, PluginStoreError::InvalidPluginName),
        (" Acme", "P", "1.0.0", PRODUCER, PluginStoreError::InvalidPluginName),
        ("Acme", "P\n", "1.0.0", PRODUCER, PluginStoreError::InvalidPluginName),
        ("Acme", "P", "1.0.0+build", PRODUCER, PluginStoreError::InvalidVersion),
        ("Acme", "P", "../1", PRODUCER, PluginStoreError::InvalidVersion),
        ("Acme", "P", "1.0.0", "", PluginStoreError::InvalidProducerVersion),
    ] {
        let name = PluginName { publisher, name: plugin };
        assert_eq!(install_plugin_version(&mut scoped, &store, name, version, &good, producer).await, Err(error));
    }
    assert_eq!(find_plugin(&mut scoped, name).await?, None);
    assert_eq!(get_manifest(&mut scoped, Uuid::new_v4(), "..").await, Err(PluginStoreError::InvalidVersion));

    // A manifest that fits in the bundle but PostgreSQL would render past the column bound.
    let expanding = format!(r#"{{"id":"org.example.junk","name":"A","main":"a","x":[{}]}}"#, vec!["1e300"; 40_000].join(","));
    assert!(expanding.len() < 256 * 1024);
    assert_eq!(
        install_plugin_version(&mut scoped, &store, name, "1.0.0", &bundle(&expanding, &[]), PRODUCER).await,
        Err(PluginStoreError::InvalidManifest(ManifestProblem::TooLarge))
    );
    scoped.rollback().await?;

    // A rejected object holding the final key blocks the install.
    let mut blocked = begin(&pool, tenant).await?;
    let installed = install_plugin_version(&mut blocked, &store, name, "1.0.0", &good, PRODUCER).await?;
    let plugin = installed.plugin.id;
    blocked.rollback().await?;
    store.delete_object(&installed.object.object_key).await?;
    let mut blocked = begin(&pool, tenant).await?;
    sqlx::query("INSERT INTO plugins (id, tenant_id, publisher, name) VALUES ($1::uuid, $2::uuid, $3, $4)")
        .bind(plugin.to_string())
        .bind(tenant.to_string())
        .bind(name.publisher)
        .bind(name.name)
        .execute(blocked.connection())
        .await?;
    let sha256 = digest(&good);
    let object = NewObject {
        id: Uuid::new_v4(),
        kind: ObjectKind::Plugin,
        object_key: keys::plugin(tenant, plugin, "1.0.0", &sha256).ok_or("key")?,
        sha256,
        size_bytes: i64::try_from(good.len())?,
        media_type: MEDIA_PLUGIN.into(),
        producer_version: PRODUCER.into(),
    };
    register_object(&mut blocked, store.bucket_name(), &object).await?;
    assert!(matches!(verify_object(&mut blocked, &store, object.id).await?, Some(Verification::Rejected(..))));
    assert_eq!(install_plugin_version(&mut blocked, &store, name, "1.0.0", &good, PRODUCER).await, Err(PluginStoreError::ObjectConflict));
    blocked.rollback().await?;

    // A pending object at the key is filled and verified; an unreachable store leaves nothing.
    let mut pending = begin(&pool, tenant).await?;
    sqlx::query("INSERT INTO plugins (id, tenant_id, publisher, name) VALUES ($1::uuid, $2::uuid, $3, $4)")
        .bind(plugin.to_string())
        .bind(tenant.to_string())
        .bind(name.publisher)
        .bind(name.name)
        .execute(pending.connection())
        .await?;
    let registered = register_object(&mut pending, store.bucket_name(), &NewObject { id: Uuid::new_v4(), ..object.clone() }).await?;
    let unreachable = ObjectStore::new(&ObjectStoreSettings { endpoint: "http://127.0.0.1:9".into(), ..settings.clone() })?;
    assert_eq!(
        install_plugin_version(&mut pending, &unreachable, name, "1.0.0", &good, PRODUCER).await,
        Err(PluginStoreError::Store(ObjectStoreError::Unreachable))
    );
    pending.rollback().await?;
    let mut pending = begin(&pool, tenant).await?;
    sqlx::query("INSERT INTO plugins (id, tenant_id, publisher, name) VALUES ($1::uuid, $2::uuid, $3, $4)")
        .bind(plugin.to_string())
        .bind(tenant.to_string())
        .bind(name.publisher)
        .bind(name.name)
        .execute(pending.connection())
        .await?;
    let registered = register_object(&mut pending, store.bucket_name(), &NewObject { id: registered.id, ..object.clone() }).await?;
    let filled = install_plugin_version(&mut pending, &store, name, "1.0.0", &good, PRODUCER).await?;
    assert_eq!((filled.plugin.id, filled.object.id, filled.object.state), (plugin, registered.id, ObjectState::Verified));
    pending.rollback().await?;
    store.delete_object(&object.object_key).await?;
    Ok(())
}

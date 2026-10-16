use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, ProjectId, TenantId, WorkspaceId};
use figcraft_cloud_relay::{DurableRoom, RoomError};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::document_snapshots::{Pointer, StoredSnapshot, store_snapshot};
use figcraft_cloud_storage::events::EventRepositoryError;
use figcraft_cloud_storage::metadata::create_document;
use figcraft_cloud_storage::objects::{ObjectStore, ObjectStoreSettings};
use figcraft_cloud_storage::validation::MEDIA_SNAPSHOT;
use figcraft_collab::message::{Access, ClientMsg};
use figcraft_collab::{Change, Client, Json, Limits, Prop, Server};
use figcraft_doc::{Document, NodeId, NodeKind};
use sqlx::postgres::{PgPool, PgPoolOptions};

const TENANT: &str = "a6000000-0000-4000-8000-000000000001";
const OTHER_TENANT: &str = "a6000000-0000-4000-8000-000000000002";
const ACTOR: &str = "a6000000-0000-4000-8000-000000000009";
const WORKSPACE: &str = "a6100000-0000-4000-8000-000000000001";
const PROJECT: &str = "a6200000-0000-4000-8000-000000000001";

type Probe = (PgPool, ObjectStoreSettings);

async fn probe() -> Result<Option<Probe>, Box<dyn std::error::Error>> {
    let (Ok(database_url_file), Ok(s3_directory)) = (std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE"), std::env::var("FIGCRAFT_TEST_S3_DIR")) else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE or FIGCRAFT_TEST_S3_DIR is unset; snapshot restore probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    let pool = PgPoolOptions::new().max_connections(4).connect(database_url.trim_end()).await?;
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

fn base() -> Result<(Document, NodeId), Box<dyn std::error::Error>> {
    let mut document = Document::new("Shared");
    let page = document.first_page().ok_or("no page")?;
    let frame = document.create(page, NodeKind::Frame, "Frame", 0.0, 0.0, 100.0, 100.0)?;
    Ok((document, frame))
}

/// A fresh document per run, so reruns never meet an earlier run's log or snapshots.
async fn new_document(pool: &PgPool) -> Result<CloudDocumentId, Box<dyn std::error::Error>> {
    let mut transaction = begin_tenant_transaction(pool, TENANT.parse()?).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let id = CloudDocumentId::random();
    let workspace: WorkspaceId = WORKSPACE.parse()?;
    let project: ProjectId = PROJECT.parse()?;
    create_document(&mut transaction, id, workspace, project, "Snapshot restore probe").await?;
    transaction.commit().await?;
    Ok(id)
}

/// Commit `names.len()` renames of `frame` through `room`.
async fn rename(room: &mut DurableRoom, frame: NodeId, names: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let ClientMsg::Hello(hello) = Client::hello("f", "", "Ada", "#FF8800") else {
        return Err("not a hello".into());
    };
    let (client, _) = room.join(hello, Access::Edit)?.map_err(|refusal| format!("join refused: {refusal:?}"))?;
    let actor: PrincipalId = ACTOR.parse()?;
    for (batch_id, name) in (1..).zip(names) {
        let changes = vec![Change::Set { id: frame, prop: Prop::Name, value: Json::of(*name)? }];
        room.receive(client, actor, ClientMsg::Changes { base_seq: room.server().seq(), batch_id, changes }).await?;
    }
    room.leave(client);
    Ok(())
}

async fn snapshot(
    pool: &PgPool,
    store: &ObjectStore,
    document: CloudDocumentId,
    server: &Server,
) -> Result<StoredSnapshot, Box<dyn std::error::Error>> {
    let mut transaction = begin_tenant_transaction(pool, TENANT.parse()?).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let stored = store_snapshot(&mut transaction, store, document, &server.state()?, "figcraft-cloud test").await?;
    transaction.commit().await?;
    Ok(stored)
}

async fn corrupt(store: &ObjectStore, key: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut bytes = store.get_object(key, 1 << 30).await?;
    let first = bytes.get_mut(1).ok_or("short object")?;
    *first ^= 0xa5;
    store.put_object(key, bytes, MEDIA_SNAPSHOT).await?;
    Ok(())
}

/// The state pure replay of the whole log over the base document produces.
async fn replayed(pool: &PgPool, tenant: TenantId, document: CloudDocumentId) -> Result<Server, Box<dyn std::error::Error>> {
    let (base, _) = base()?;
    let room = DurableRoom::open(pool.clone(), tenant, document, base, "", Limits::default()).await?;
    Ok(Server::restore(room.server().state()?, "", Limits::default())?)
}

#[tokio::test]
async fn rooms_recover_from_the_latest_verified_snapshot() -> Result<(), Box<dyn std::error::Error>> {
    let Some((pool, settings)) = probe().await? else {
        return Ok(());
    };
    let store = ObjectStore::new(&settings)?;
    let tenant: TenantId = TENANT.parse()?;
    let document = new_document(&pool).await?;
    let (doc, frame) = base()?;
    let mut keys = Vec::new();

    // With no snapshot, recovery is the base-document path.
    let mut room = DurableRoom::recover(pool.clone(), &store, tenant, document, doc, "", Limits::default()).await?;
    assert_eq!(room.restored_snapshot(), None);
    rename(&mut room, frame, &["One", "Two", "Three"]).await?;
    assert_eq!(room.server().seq(), 3);
    let at_three = snapshot(&pool, &store, document, room.server()).await?;
    keys.push(at_three.object.object_key.clone());
    assert_eq!((at_three.snapshot.seq, at_three.pointer), (3, Pointer::Moved));
    rename(&mut room, frame, &["Four", "Five"]).await?;

    // Snapshot plus the events after it equals pure replay. The base is never read: a base the
    // log does not apply over still recovers, because only events 4 and 5 are replayed.
    let wrong_base = Document::new("Without the frame");
    let recovered = DurableRoom::recover(pool.clone(), &store, tenant, document, wrong_base, "", Limits::default()).await?;
    assert_eq!(recovered.restored_snapshot(), Some((at_three.snapshot.id, 3)));
    let pure = replayed(&pool, tenant, document).await?;
    assert_eq!((recovered.server().replica(), recovered.server().seq()), (pure.replica(), 5));
    assert_eq!(recovered.server().document().node(frame).map(|node| node.name.as_str()), Some("Five"));
    assert!(matches!(
        DurableRoom::open(pool.clone(), tenant, document, Document::new("Without the frame"), "", Limits::default()).await,
        Err(RoomError::Replay)
    ));

    // A newer snapshot is preferred; once its object is corrupted in the bucket it is never used
    // and recovery falls back to the preceding one.
    let mut room = DurableRoom::recover(pool.clone(), &store, tenant, document, base()?.0, "", Limits::default()).await?;
    rename(&mut room, frame, &["Six"]).await?;
    let at_six = snapshot(&pool, &store, document, room.server()).await?;
    keys.push(at_six.object.object_key.clone());
    rename(&mut room, frame, &["Seven"]).await?;
    let recovered = DurableRoom::recover(pool.clone(), &store, tenant, document, base()?.0, "", Limits::default()).await?;
    assert_eq!(recovered.restored_snapshot(), Some((at_six.snapshot.id, 6)));
    corrupt(&store, &at_six.object.object_key).await?;
    let recovered = DurableRoom::recover(pool.clone(), &store, tenant, document, base()?.0, "", Limits::default()).await?;
    assert_eq!(recovered.restored_snapshot(), Some((at_three.snapshot.id, 3)));
    let pure = replayed(&pool, tenant, document).await?;
    assert_eq!((recovered.server().replica(), recovered.server().seq()), (pure.replica(), 7));

    // With every snapshot corrupt, the base is the preceding snapshot; a base the log does not
    // apply over is refused rather than served.
    corrupt(&store, &at_three.object.object_key).await?;
    let recovered = DurableRoom::recover(pool.clone(), &store, tenant, document, base()?.0, "", Limits::default()).await?;
    assert_eq!(recovered.restored_snapshot(), None);
    assert_eq!(recovered.server().replica(), replayed(&pool, tenant, document).await?.replica());
    assert!(matches!(
        DurableRoom::recover(pool.clone(), &store, tenant, document, Document::new("Without the frame"), "", Limits::default()).await,
        Err(RoomError::Replay)
    ));

    // Another tenant cannot open the document at all, so none of its snapshots are read.
    assert!(matches!(
        DurableRoom::recover(pool.clone(), &store, OTHER_TENANT.parse()?, document, base()?.0, "", Limits::default()).await,
        Err(RoomError::Open(EventRepositoryError::NotFound))
    ));

    for key in keys {
        store.delete_object(&key).await?;
    }
    pool.close().await;
    Ok(())
}

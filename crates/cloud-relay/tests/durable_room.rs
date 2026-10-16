use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, TenantId};
use figcraft_cloud_relay::{DurableRoom, RoomError};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::events::{EventRepositoryError, claim_sequencer};
use figcraft_collab::message::{Access, ClientMsg};
use figcraft_collab::{Change, Client, Json, Limits, Outgoing, Prop, Server, ServerMsg};
use figcraft_doc::{Document, NodeId, NodeKind};
use sqlx::postgres::{PgPool, PgPoolOptions};

const TENANT: &str = "97000000-0000-4000-8000-000000000001";
const ACTOR: &str = "97000000-0000-4000-8000-000000000009";
const DOCUMENT: &str = "97300000-0000-4000-8000-000000000001";

async fn pool() -> Result<Option<PgPool>, Box<dyn std::error::Error>> {
    let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; PostgreSQL durable room probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    Ok(Some(PgPoolOptions::new().max_connections(4).connect(database_url.trim_end()).await?))
}

fn document() -> Result<(Document, NodeId), Box<dyn std::error::Error>> {
    let mut document = Document::new("Shared");
    let page = document.first_page().ok_or("no page")?;
    let frame = document.create(page, NodeKind::Frame, "Frame", 0.0, 0.0, 100.0, 100.0)?;
    Ok((document, frame))
}

fn rename(server: &Server, frame: NodeId, batch_id: u64, name: &str) -> Result<ClientMsg, Box<dyn std::error::Error>> {
    Ok(ClientMsg::Changes { base_seq: server.seq(), batch_id, changes: vec![Change::Set { id: frame, prop: Prop::Name, value: Json::of(name)? }] })
}

async fn committed(pool: &PgPool) -> Result<(i64, i64), Box<dyn std::error::Error>> {
    let mut scoped = begin_tenant_transaction(pool, TENANT.parse()?).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let row: (i64, i64) = sqlx::query_as(
        "SELECT d.current_seq, (SELECT count(*) FROM document_events e WHERE e.tenant_id = d.tenant_id AND e.document_id = d.id) \
         FROM documents d WHERE d.tenant_id = $1::uuid AND d.id = $2::uuid",
    )
    .bind(TENANT)
    .bind(DOCUMENT)
    .fetch_one(scoped.connection())
    .await?;
    scoped.rollback().await?;
    Ok(row)
}

async fn claim(pool: &PgPool) -> Result<i64, Box<dyn std::error::Error>> {
    let mut scoped = begin_tenant_transaction(pool, TENANT.parse()?).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    let claim = claim_sequencer(&mut scoped, DOCUMENT.parse()?).await?;
    scoped.commit().await?;
    Ok(claim.fencing_token)
}

#[tokio::test]
async fn accepted_batches_are_committed_before_they_are_confirmed() -> Result<(), Box<dyn std::error::Error>> {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let tenant: TenantId = TENANT.parse()?;
    let document_id: CloudDocumentId = DOCUMENT.parse()?;
    let actor: PrincipalId = ACTOR.parse()?;
    let (before, events_before) = committed(&pool).await?;
    assert_eq!(before, events_before, "fixture starts with a gap-free log");
    // A fresh fixture has no events; this probe builds its in-memory server from an empty log.
    assert_eq!(before, 0, "run the durable room fixture before this probe");

    let (doc, frame) = document()?;
    let token = claim(&pool).await?;
    let mut room = DurableRoom::from_parts(pool.clone(), tenant, document_id, token, Server::new(doc, "", Limits::default()));
    let ClientMsg::Hello(hello) = Client::hello("f", "", "Ada", "#FF8800") else {
        return Err("not a hello".into());
    };
    let (client, welcome) = room.join(hello, Access::Edit)?.map_err(|refusal| format!("join refused: {refusal:?}"))?;
    assert!(!welcome.is_empty());
    assert_eq!(committed(&pool).await?, (0, 0), "joining commits nothing");

    // An accepted batch is durable by the time its broadcast is handed back.
    let message = rename(room.server(), frame, 1, "One")?;
    let out = room.receive(client, actor, message).await?;
    assert!(out.iter().any(|outgoing| matches!(outgoing, Outgoing::All(ServerMsg::Changes(batch)) if batch.seq == 1)));
    assert_eq!(committed(&pool).await?, (1, 1));

    // A refused batch writes nothing and is still answered.
    let out = room.receive(client, actor, ClientMsg::Changes { base_seq: 99, batch_id: 2, changes: vec![] }).await?;
    assert!(out.iter().any(|outgoing| matches!(outgoing, Outgoing::To(to, ServerMsg::Reject { .. }) if *to == client)));
    assert_eq!(committed(&pool).await?, (1, 1));

    // Another sequencer takes over. The old room's next accepted batch is fenced: nothing is
    // returned to deliver, nothing is written and the room refuses everything after.
    claim(&pool).await?;
    let message = rename(room.server(), frame, 3, "Two")?;
    assert_eq!(room.receive(client, actor, message).await, Err(RoomError::Append(EventRepositoryError::StaleFence)));
    assert!(room.is_poisoned());
    assert_eq!(committed(&pool).await?, (1, 1));
    let message = rename(room.server(), frame, 4, "Three")?;
    assert_eq!(room.receive(client, actor, message).await, Err(RoomError::Poisoned));
    let ClientMsg::Hello(hello) = Client::hello("f", "", "Bo", "#0088FF") else {
        return Err("not a hello".into());
    };
    assert!(matches!(room.join(hello, Access::Edit), Err(RoomError::Poisoned)));
    assert_eq!(committed(&pool).await?, (1, 1));

    // A restarted room claims the document, fencing every earlier owner, and replays the
    // committed log: it holds exactly what was confirmed, never the fenced batch.
    let (base, _) = document()?;
    let mut reopened = DurableRoom::open(pool.clone(), tenant, document_id, base, "", Limits::default()).await?;
    assert_eq!(reopened.server().seq(), 1);
    assert!(reopened.fencing_token() > room.fencing_token());
    assert_eq!(reopened.server().document().node(frame).map(|node| node.name.as_str()), Some("One"));
    let ClientMsg::Hello(hello) = Client::hello("f", "", "Cy", "#00FF88") else {
        return Err("not a hello".into());
    };
    let (client, _) = reopened.join(hello, Access::Edit)?.map_err(|refusal| format!("join refused: {refusal:?}"))?;
    let message = rename(reopened.server(), frame, 1, "Four")?;
    reopened.receive(client, actor, message).await?;
    assert_eq!(committed(&pool).await?, (2, 2));

    // A base the committed batches do not apply over is refused rather than served.
    let wrong_base = Document::new("Without the frame");
    assert!(matches!(DurableRoom::open(pool.clone(), tenant, document_id, wrong_base, "", Limits::default()).await, Err(RoomError::Replay)));

    pool.close().await;
    Ok(())
}

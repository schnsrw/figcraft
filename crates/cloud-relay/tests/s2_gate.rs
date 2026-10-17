//! S2 gate: create, edit, restart and restore a document using PostgreSQL metadata and events.
//!
//! One end-to-end probe over public APIs only. The workspace, project and document are created
//! through the metadata repositories; three collaboration clients (two editors and a viewer) edit
//! through a `DurableRoom`; the room is dropped and reopened from the committed log; two commits
//! are made to fail (a fenced sequencer and a connection terminated mid-append) and neither batch
//! is confirmed, written or replayed; and the document is renamed, archived (an archived document
//! refuses edits and cannot be opened) and restored with its state intact. Every confirmed batch is
//! exactly the durable log, in order.
//!
//! The fixture (`deploy/saas/s2-gate-preflight.sql`) holds only the tenant and its principal; the
//! cleanup removes everything under the tenant, so the probe reruns after each cleanup.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::{Duration, Instant};

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, ProjectId, ResourceState, TenantId, Uuid, WorkspaceId};
use figcraft_cloud_relay::{DurableRoom, RoomError};
use figcraft_cloud_storage::audit::record_audit_event;
use figcraft_cloud_storage::events::{EventRepositoryError, MAX_REPLAY_BYTES, MAX_REPLAY_EVENTS, read_events};
use figcraft_cloud_storage::metadata::{
    archive_document, create_document, create_project, create_workspace, get_document, rename_document, restore_document,
};
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use figcraft_collab::message::{Access, ClientMsg};
use figcraft_collab::{Accepted, Client, Limits, NoRecompute, Outgoing, ServerMsg};
use figcraft_doc::{Document, NodeId, NodeKind};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const TENANT: &str = "99000000-0000-4000-8000-000000000001";
const ACTOR: &str = "99000000-0000-4000-8000-000000000009";
const WORKSPACE: &str = "99100000-0000-4000-8000-000000000001";
const PROJECT: &str = "99200000-0000-4000-8000-000000000001";
const DOCUMENT: &str = "99300000-0000-4000-8000-000000000001";

fn database_url() -> TestResult<Option<String>> {
    let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; PostgreSQL S2 gate probe skipped");
        return Ok(None);
    };
    Ok(Some(std::fs::read_to_string(database_url_file)?.trim_end().to_string()))
}

/// The document at sequence 0. Built the same way every time, so a restart replays over the same
/// base the first room started from.
fn base() -> TestResult<(Document, NodeId, NodeId)> {
    let mut document = Document::new("S2 gate");
    let page = document.first_page().ok_or("no page")?;
    let frame = document.create(page, NodeKind::Frame, "Frame", 0.0, 0.0, 400.0, 300.0)?;
    Ok((document, page, frame))
}

async fn scoped(pool: &PgPool) -> TestResult<TenantTransaction<'_>> {
    begin_tenant_transaction(pool, TENANT.parse()?).await.map_err(|error| format!("begin tenant: {error:?}").into())
}

/// The committed `(current_seq, event count)` of the gate document.
async fn committed(pool: &PgPool) -> TestResult<(i64, i64)> {
    let mut scoped = scoped(pool).await?;
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

/// Every committed batch, in order, page by page.
async fn durable_log(pool: &PgPool) -> TestResult<Vec<Accepted>> {
    let mut log = Vec::new();
    let mut after = 0;
    loop {
        let mut scoped = scoped(pool).await?;
        let page = read_events(&mut scoped, DOCUMENT.parse()?, after, MAX_REPLAY_EVENTS, MAX_REPLAY_BYTES).await?;
        scoped.rollback().await?;
        let more = page.has_more();
        after = page.next_after_seq();
        log.extend(page.events.into_iter().map(|event| event.batch));
        if !more {
            return Ok(log);
        }
    }
}

async fn audit(pool: &PgPool, action: &str, name: &str) -> TestResult {
    let mut scoped = scoped(pool).await?;
    let actor: PrincipalId = ACTOR.parse()?;
    let document: CloudDocumentId = DOCUMENT.parse()?;
    let metadata = serde_json::json!({ "name": name });
    record_audit_event(
        &mut scoped,
        Uuid::new_v4(),
        Some(actor.as_uuid()),
        action,
        "document",
        Some(document.as_uuid()),
        None,
        None,
        "succeeded",
        &metadata,
    )
    .await?;
    scoped.commit().await?;
    Ok(())
}

/// The connected clients, keyed by the id the room issued them, and every batch the room
/// confirmed (broadcast) to them.
#[derive(Default)]
struct Net {
    clients: BTreeMap<u32, Client>,
    confirmed: Vec<Accepted>,
}

impl Net {
    /// Deliver what the room returned; return what the clients answer with.
    fn deliver(&mut self, out: Vec<Outgoing>) -> TestResult<Vec<(u32, ClientMsg)>> {
        let mut answers = Vec::new();
        for outgoing in out {
            let (to, msg): (Vec<u32>, ServerMsg) = match outgoing {
                Outgoing::To(id, msg) => (vec![id], msg),
                Outgoing::All(msg) => (self.clients.keys().copied().collect(), msg),
                Outgoing::Others(but, msg) => (self.clients.keys().copied().filter(|id| *id != but).collect(), msg),
            };
            if let ServerMsg::Changes(batch) = &msg {
                self.confirmed.push(batch.clone());
            }
            for id in to {
                let client = self.clients.get_mut(&id).ok_or("message for an unknown client")?;
                for answer in client.receive(msg.clone(), &NoRecompute)? {
                    answers.push((id, answer));
                }
            }
        }
        Ok(answers)
    }

    async fn join(&mut self, room: &mut DurableRoom, name: &str, access: Access) -> TestResult<u32> {
        let ClientMsg::Hello(hello) = Client::hello("s2-gate", "", name, "#33AA77") else {
            return Err("not a hello".into());
        };
        let (id, out) = room.join(hello, access)?.map_err(|refusal| format!("join refused: {refusal:?}"))?;
        let mut rest = Vec::new();
        let mut welcome = None;
        for outgoing in out {
            match outgoing {
                Outgoing::To(to, msg @ ServerMsg::Welcome { .. }) if to == id => welcome = Some(msg),
                other => rest.push(other),
            }
        }
        self.clients.insert(id, Client::join(welcome.ok_or("no welcome")?)?);
        let answers = self.deliver(rest)?;
        assert!(answers.is_empty(), "a join needs no answer");
        Ok(id)
    }

    /// `id` edits its document with `edit`; return the batch its client would send.
    fn edit(&mut self, id: u32, edit: impl FnOnce(&mut Document) -> TestResult) -> TestResult<ClientMsg> {
        let client = self.clients.get_mut(&id).ok_or("unknown client")?;
        let mut document = client.document().clone();
        edit(&mut document)?;
        Ok(client.edit(&document)?.ok_or("the edit changed nothing")?)
    }

    /// Send `msg` from `from` through the room and deliver everything it causes, follow-ups
    /// included.
    async fn send(&mut self, room: &mut DurableRoom, from: u32, msg: ClientMsg) -> TestResult {
        let actor: PrincipalId = ACTOR.parse()?;
        let mut queue = vec![(from, msg)];
        while let Some((from, msg)) = queue.pop() {
            let out = room.receive(from, actor, msg).await?;
            queue.extend(self.deliver(out)?);
        }
        Ok(())
    }

    /// Every client's confirmed replica matches the room's state.
    fn converged(&self, room: &DurableRoom) -> TestResult {
        let server = room.server().replica().snapshot()?;
        for (id, client) in &self.clients {
            assert_eq!(client.confirmed().snapshot()?, server, "client {id} diverged from the room");
            assert_eq!(client.pending(), 0, "client {id} still waits for a batch");
        }
        Ok(())
    }
}

fn rename_node(node: NodeId, name: &'static str) -> impl FnOnce(&mut Document) -> TestResult {
    move |document| {
        document.node_mut(node).ok_or("no such node")?.name = name.to_string();
        Ok(())
    }
}

async fn open(pool: &PgPool) -> Result<DurableRoom, RoomError> {
    let Ok((document, _, _)) = base() else {
        return Err(RoomError::Replay);
    };
    let (Ok(tenant), Ok(id)) = (TENANT.parse::<TenantId>(), DOCUMENT.parse::<CloudDocumentId>()) else {
        return Err(RoomError::Replay);
    };
    DurableRoom::open(pool.clone(), tenant, id, document, "", Limits::default()).await
}

/// Wait until the connection named `application_name` is blocked on a lock, then terminate it.
async fn terminate_when_blocked(pool: &PgPool, application_name: &str) -> TestResult {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let blocked: Option<(i32,)> = sqlx::query_as("SELECT pid FROM pg_stat_activity WHERE application_name = $1 AND wait_event_type = 'Lock'")
            .bind(application_name)
            .fetch_optional(pool)
            .await?;
        if let Some((pid,)) = blocked {
            let (terminated,): (bool,) = sqlx::query_as("SELECT pg_terminate_backend($1)").bind(pid).fetch_one(pool).await?;
            assert!(terminated, "the room's backend was not terminated");
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err("the room never blocked on the document row".into());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn create_edit_restart_and_restore_a_document() -> TestResult {
    let Some(database_url) = database_url()? else {
        return Ok(());
    };
    let pool = PgPoolOptions::new().max_connections(4).connect(&database_url).await?;
    let document_id: CloudDocumentId = DOCUMENT.parse()?;

    // Create: workspace, project and document through the metadata repositories.
    let mut tx = scoped(&pool).await?;
    let workspace = create_workspace(&mut tx, WORKSPACE.parse::<WorkspaceId>()?, "Gate workspace", None).await?;
    let project = create_project(&mut tx, PROJECT.parse::<ProjectId>()?, workspace.id, "Gate project", None).await?;
    let record = create_document(&mut tx, document_id, workspace.id, project.id, "Gate document").await?;
    tx.commit().await?;
    assert_eq!(record.state, ResourceState::Active);
    audit(&pool, "document.create", "Gate document").await?;
    assert_eq!(committed(&pool).await?, (0, 0), "a new document has an empty log");

    // Edit: two editors and a viewer in one room. Creates and property sets from both editors are
    // confirmed; the viewer's batch is refused and writes nothing.
    let (_, page, frame) = base()?;
    let mut room = open(&pool).await?;
    assert_eq!(room.server().seq(), 0);
    let mut net = Net::default();
    let ada = net.join(&mut room, "Ada", Access::Edit).await?;
    let bo = net.join(&mut room, "Bo", Access::Edit).await?;
    let cy = net.join(&mut room, "Cy", Access::View).await?;
    assert_eq!(committed(&pool).await?, (0, 0), "joining commits nothing");

    let mut card = None;
    let msg = net.edit(ada, |document| {
        card = Some(document.create(page, NodeKind::Rectangle, "Card", 20.0, 20.0, 120.0, 80.0)?);
        Ok(())
    })?;
    let card = card.ok_or("no card")?;
    net.send(&mut room, ada, msg).await?;
    let msg = net.edit(bo, rename_node(frame, "Gate frame"))?;
    net.send(&mut room, bo, msg).await?;
    let mut badge = None;
    let msg = net.edit(bo, |document| {
        badge = Some(document.create(frame, NodeKind::Rectangle, "Badge", 10.0, 10.0, 24.0, 24.0)?);
        Ok(())
    })?;
    let badge = badge.ok_or("no badge")?;
    net.send(&mut room, bo, msg).await?;
    let msg = net.edit(ada, |document| {
        let node = document.node_mut(card).ok_or("no card")?;
        node.opacity = 0.5;
        node.visible = false;
        Ok(())
    })?;
    net.send(&mut room, ada, msg).await?;
    let before_refusal = net.confirmed.len();
    let msg = net.edit(cy, rename_node(frame, "Viewer was here"))?;
    net.send(&mut room, cy, msg).await?;
    assert_eq!(net.confirmed.len(), before_refusal, "the viewer's batch is refused, not confirmed");
    let msg = net.edit(ada, rename_node(badge, "Badge 2"))?;
    net.send(&mut room, ada, msg).await?;

    let accepted = i64::try_from(net.confirmed.len())?;
    assert_eq!(accepted, 5);
    assert_eq!(committed(&pool).await?, (accepted, accepted), "every confirmed batch is committed, and nothing else");
    assert_eq!(durable_log(&pool).await?, net.confirmed, "the durable log is exactly what was confirmed, in order");
    net.converged(&room)?;
    let edited = room.server().replica().snapshot()?;
    assert_eq!(room.server().document().node(frame).map(|node| node.name.as_str()), Some("Gate frame"));

    // Restart: drop the room and reopen it over the same base. Replay reaches the confirmed state.
    let first_token = room.fencing_token();
    drop(room);
    let mut net = Net { clients: BTreeMap::new(), confirmed: net.confirmed };
    let mut stale = open(&pool).await?;
    assert!(stale.fencing_token() > first_token);
    assert_eq!(stale.server().replica().snapshot()?, edited, "the restored document equals the one before the restart");
    assert_eq!(i64::try_from(stale.server().seq())?, committed(&pool).await?.0);
    let ada = net.join(&mut stale, "Ada", Access::Edit).await?;
    net.converged(&stale)?;

    // A failed commit is never confirmed (1): another sequencer opens the document, so this
    // room's next accepted batch is fenced. Nothing is returned to deliver and nothing is written.
    let application_name = format!("figcraft-s2-gate-room-{}", Uuid::new_v4().simple());
    let victim_pool =
        PgPoolOptions::new().max_connections(1).connect_with(PgConnectOptions::from_str(&database_url)?.application_name(&application_name)).await?;
    let mut victim = open(&victim_pool).await?;
    let msg = net.edit(ada, rename_node(frame, "Fenced"))?;
    let actor: PrincipalId = ACTOR.parse()?;
    assert_eq!(stale.receive(ada, actor, msg).await, Err(RoomError::Append(EventRepositoryError::StaleFence)));
    assert!(stale.is_poisoned());
    assert_eq!(committed(&pool).await?, (accepted, accepted));
    drop(stale);

    // A failed commit is never confirmed (2): the room's database connection is terminated while
    // its append waits on the document row, so the transaction never commits.
    let mut net = Net { clients: BTreeMap::new(), confirmed: net.confirmed };
    let ada = net.join(&mut victim, "Ada", Access::Edit).await?;
    let msg = net.edit(ada, rename_node(card, "Lost"))?;
    let mut blocker = scoped(&pool).await?;
    sqlx::query("SELECT id FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid FOR UPDATE")
        .bind(TENANT)
        .bind(DOCUMENT)
        .fetch_one(blocker.connection())
        .await?;
    let (received, terminated) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(30), victim.receive(ada, actor, msg)),
        terminate_when_blocked(&pool, &application_name)
    );
    blocker.rollback().await?;
    terminated?;
    let received = received.map_err(|_| "the room's receive never finished")?;
    assert!(
        matches!(received, Err(RoomError::Append(EventRepositoryError::Unavailable))),
        "a terminated append is an unavailable error: {received:?}"
    );
    assert!(victim.is_poisoned());
    assert_eq!(committed(&pool).await?, (accepted, accepted), "the terminated batch is not committed");
    assert_eq!(durable_log(&pool).await?, net.confirmed, "neither failed batch is in the log");
    drop(victim);
    victim_pool.close().await;

    // A fresh open excludes both failed batches and includes every confirmed one.
    let mut room = open(&pool).await?;
    assert_eq!(room.server().replica().snapshot()?, edited);
    assert_eq!(room.server().document().node(frame).map(|node| node.name.as_str()), Some("Gate frame"));
    assert_eq!(room.server().document().node(card).map(|node| node.name.as_str()), Some("Card"));
    let mut net = Net { clients: BTreeMap::new(), confirmed: net.confirmed };
    let ada = net.join(&mut room, "Ada", Access::Edit).await?;
    let bo = net.join(&mut room, "Bo", Access::Edit).await?;
    let msg = net.edit(bo, rename_node(card, "Card after recovery"))?;
    net.send(&mut room, bo, msg).await?;
    let accepted = i64::try_from(net.confirmed.len())?;
    assert_eq!(accepted, 6);
    assert_eq!(committed(&pool).await?, (accepted, accepted));
    net.converged(&room)?;
    let recovered = room.server().replica().snapshot()?;

    // Rename and archive through the metadata repositories. An archived document refuses edits
    // (the live room's commit fails and nothing is confirmed) and cannot be opened.
    let mut tx = scoped(&pool).await?;
    let renamed = rename_document(&mut tx, document_id, "Gate document (renamed)").await?.ok_or("no document to rename")?;
    tx.commit().await?;
    assert_eq!(renamed.name, "Gate document (renamed)");
    audit(&pool, "document.rename", &renamed.name).await?;
    let mut tx = scoped(&pool).await?;
    let archived = archive_document(&mut tx, document_id).await?.ok_or("no document to archive")?;
    tx.commit().await?;
    assert_eq!(archived.state, ResourceState::Archived);
    audit(&pool, "document.archive", &archived.name).await?;

    let msg = net.edit(ada, rename_node(frame, "Archived edit"))?;
    assert_eq!(room.receive(ada, actor, msg).await, Err(RoomError::Append(EventRepositoryError::InvalidState)));
    drop(room);
    assert!(matches!(open(&pool).await, Err(RoomError::Open(EventRepositoryError::InvalidState))));
    assert_eq!(committed(&pool).await?, (accepted, accepted));

    // Restore: the document opens again with its name and every confirmed batch intact.
    let mut tx = scoped(&pool).await?;
    let restored = restore_document(&mut tx, document_id).await?.ok_or("no document to restore")?;
    tx.commit().await?;
    assert_eq!(restored.state, ResourceState::Active);
    audit(&pool, "document.restore", &restored.name).await?;
    let room = open(&pool).await?;
    assert_eq!(room.server().replica().snapshot()?, recovered, "restore keeps the committed state");
    assert_eq!(i64::try_from(room.server().seq())?, accepted);
    assert_eq!(room.server().document().node(frame).map(|node| node.name.as_str()), Some("Gate frame"));
    assert_eq!(durable_log(&pool).await?, net.confirmed);
    assert!(net.confirmed.iter().zip(1..).all(|(batch, seq)| batch.seq == seq), "confirmed sequences are gap-free");

    let mut tx = scoped(&pool).await?;
    let current = get_document(&mut tx, document_id).await?.ok_or("no document")?;
    let audited: (i64,) =
        sqlx::query_as("SELECT count(*) FROM audit_events WHERE tenant_id = $1::uuid AND resource_kind = 'document' AND resource_id = $2::uuid")
            .bind(TENANT)
            .bind(DOCUMENT)
            .fetch_one(tx.connection())
            .await?;
    tx.rollback().await?;
    assert_eq!((current.name.as_str(), current.state), ("Gate document (renamed)", ResourceState::Active));
    assert_eq!(audited.0, 4, "create, rename, archive and restore are audited");

    pool.close().await;
    Ok(())
}

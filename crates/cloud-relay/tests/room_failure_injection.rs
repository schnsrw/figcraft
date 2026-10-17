//! S2-020: a durable room whose database connection dies mid-receive.
//!
//! The room runs on its own one-connection pool tagged with a per-run `application_name`, so the
//! test can find exactly the room's backend in `pg_stat_activity` and terminate it from a second
//! runtime connection. To hit the window inside `receive`, a gate transaction holds the document
//! row lock, the room's append queues on it, and the test kills the room's backend while it waits.
//! Every wait polls against a deadline; no sleep stands in for synchronisation.

use std::str::FromStr;
use std::time::{Duration, Instant};

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, TenantId};
use figcraft_cloud_relay::{DurableRoom, RoomError};
use figcraft_cloud_storage::events::{EventRepositoryError, MAX_REPLAY_BYTES, read_events};
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use figcraft_collab::message::{Access, ClientMsg};
use figcraft_collab::{Change, Client, Json, Limits, Outgoing, Prop, Server, ServerMsg};
use figcraft_doc::{Document, NodeId, NodeKind};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const TENANT: &str = "98000000-0000-4000-8000-000000000001";
const ACTOR: &str = "98000000-0000-4000-8000-000000000009";
const DOCUMENT: &str = "98300000-0000-4000-8000-000000000005";

/// Upper bound on any single wait, so a lost wake-up fails the test instead of hanging it.
const BOUND: Duration = Duration::from_secs(30);

/// The room's own pool (one connection, tagged with `application_name`) and a control pool for
/// the gate, the poller and the killer.
async fn pools(application_name: &str) -> TestResult<Option<(PgPool, PgPool)>> {
    let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; PostgreSQL room failure-injection probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    let options = PgConnectOptions::from_str(database_url.trim_end())?;
    let room =
        PgPoolOptions::new().max_connections(1).acquire_timeout(BOUND).connect_with(options.clone().application_name(application_name)).await?;
    let control = PgPoolOptions::new().max_connections(3).acquire_timeout(BOUND).connect_with(options).await?;
    Ok(Some((room, control)))
}

fn document() -> TestResult<(Document, NodeId)> {
    let mut document = Document::new("Shared");
    let page = document.first_page().ok_or("no page")?;
    let frame = document.create(page, NodeKind::Frame, "Frame", 0.0, 0.0, 100.0, 100.0)?;
    Ok((document, frame))
}

fn rename(server: &Server, frame: NodeId, batch_id: u64, name: &str) -> TestResult<ClientMsg> {
    Ok(ClientMsg::Changes { base_seq: server.seq(), batch_id, changes: vec![Change::Set { id: frame, prop: Prop::Name, value: Json::of(name)? }] })
}

async fn begin(pool: &PgPool) -> TestResult<TenantTransaction<'_>> {
    let tenant: TenantId = TENANT.parse()?;
    let mut transaction = begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}"))?;
    sqlx::query("SET LOCAL lock_timeout = '20s'").execute(transaction.connection()).await?;
    Ok(transaction)
}

/// The document's committed sequence and its event-row count.
async fn committed(pool: &PgPool) -> TestResult<(i64, i64)> {
    let mut transaction = begin(pool).await?;
    let row: (i64, i64) = sqlx::query_as(
        "SELECT d.current_seq, (SELECT count(*) FROM document_events e WHERE e.tenant_id = d.tenant_id AND e.document_id = d.id) \
         FROM documents d WHERE d.tenant_id = $1::uuid AND d.id = $2::uuid",
    )
    .bind(TENANT)
    .bind(DOCUMENT)
    .fetch_one(transaction.connection())
    .await?;
    transaction.rollback().await?;
    Ok(row)
}

/// The room's backend while it sits idle in its pool.
const IDLE: &str = "SELECT pid FROM pg_stat_activity WHERE application_name = $1 AND usename = current_user AND state = 'idle'";
/// The room's backend while it is queued on a heavyweight lock.
const LOCK_WAIT: &str = "SELECT pid FROM pg_stat_activity WHERE application_name = $1 AND usename = current_user AND wait_event_type = 'Lock'";

/// The room's backend, once `query` ([`IDLE`] or [`LOCK_WAIT`]) finds it.
async fn room_backend(control: &PgPool, application_name: &str, query: &'static str) -> TestResult<i32> {
    let deadline = Instant::now() + BOUND;
    loop {
        let pids: Vec<i32> = sqlx::query_scalar(query).bind(application_name).fetch_all(control).await?;
        match pids.as_slice() {
            [pid] => return Ok(*pid),
            [] if Instant::now() <= deadline => tokio::time::sleep(Duration::from_millis(5)).await,
            [] => return Err(format!("the room's backend never matched {query}").into()),
            _ => return Err(format!("the room's one-connection pool has {} backends", pids.len()).into()),
        }
    }
}

/// Terminate `pid` and wait until it has exited.
async fn kill(control: &PgPool, pid: i32) -> TestResult {
    let signalled: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)").bind(pid).fetch_one(control).await?;
    assert!(signalled, "backend {pid} could not be terminated");
    let deadline = Instant::now() + BOUND;
    loop {
        let alive: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1)").bind(pid).fetch_one(control).await?;
        if !alive {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(format!("backend {pid} never exited").into());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn a_room_whose_connection_dies_mid_receive_poisons_and_reopens_from_committed_state() -> TestResult {
    let application_name = format!("s2020-room-{}", CloudDocumentId::random());
    let Some((room_pool, control)) = pools(&application_name).await? else {
        return Ok(());
    };
    let tenant: TenantId = TENANT.parse()?;
    let document_id: CloudDocumentId = DOCUMENT.parse()?;
    let actor: PrincipalId = ACTOR.parse()?;

    // The fixture document keeps earlier runs' renames, all of which replay over the same base.
    let (base, frame) = document()?;
    let mut room = DurableRoom::open(room_pool.clone(), tenant, document_id, base, "", Limits::default()).await?;
    let (start, rows) = committed(&control).await?;
    assert_eq!(start, rows, "fixture log is gap-free");
    assert_eq!(room.server().seq(), u64::try_from(start)?);
    let ClientMsg::Hello(hello) = Client::hello("f", "", "Ada", "#FF8800") else {
        return Err("not a hello".into());
    };
    let (client, _) = room.join(hello, Access::Edit)?.map_err(|refusal| format!("join refused: {refusal:?}"))?;

    let message = rename(room.server(), frame, 1, "Committed one")?;
    room.receive(client, actor, message).await?;
    assert_eq!(committed(&control).await?, (start + 1, start + 1));

    // A connection that died while idle in the pool is not a failure: the pool replaces it before
    // the next commit, and the room carries on.
    let idle = room_backend(&control, &application_name, IDLE).await?;
    kill(&control, idle).await?;
    let message = rename(room.server(), frame, 2, "Committed two")?;
    let out = room.receive(client, actor, message).await?;
    assert!(out.iter().any(|outgoing| matches!(outgoing, Outgoing::All(ServerMsg::Changes(batch)) if batch.batch_id == 2)));
    assert!(!room.is_poisoned());
    let durable = start + 2;
    assert_eq!(committed(&control).await?, (durable, durable));

    // The connection dies while the room's append is queued on the document row: the server has
    // accepted the batch in memory, but it never commits, so nothing may be delivered.
    let mut gate = begin(&control).await?;
    sqlx::query("SELECT 1 FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid FOR UPDATE")
        .bind(TENANT)
        .bind(DOCUMENT)
        .fetch_one(gate.connection())
        .await?;
    let message = rename(room.server(), frame, 3, "Lost")?;
    let killer = async {
        let victim = room_backend(&control, &application_name, LOCK_WAIT).await?;
        kill(&control, victim).await?;
        gate.rollback().await?;
        TestResult::Ok(())
    };
    let (received, killed) = tokio::time::timeout(BOUND, async { tokio::join!(room.receive(client, actor, message), killer) }).await?;
    killed?;
    assert_eq!(received, Err(RoomError::Append(EventRepositoryError::Unavailable)), "the dead connection surfaces as an unavailable append");
    assert!(room.is_poisoned());
    assert_eq!(room.server().seq(), u64::try_from(durable + 1)?, "the room's memory is ahead of durable state");
    assert_eq!(committed(&control).await?, (durable, durable), "the lost batch is not durable");
    let message = rename(room.server(), frame, 4, "After")?;
    assert_eq!(room.receive(client, actor, message).await, Err(RoomError::Poisoned));
    assert_eq!(committed(&control).await?, (durable, durable));

    // Reopening on the same pool replays only what committed: the lost rename is gone.
    let (base, _) = document()?;
    let reopened = DurableRoom::open(room_pool.clone(), tenant, document_id, base, "", Limits::default()).await?;
    assert_eq!(reopened.server().seq(), u64::try_from(durable)?);
    assert!(reopened.fencing_token() > room.fencing_token());
    assert_eq!(reopened.server().document().node(frame).map(|node| node.name.as_str()), Some("Committed two"));
    let mut transaction = begin(&control).await?;
    let page = read_events(&mut transaction, document_id, durable - 1, 10, MAX_REPLAY_BYTES).await?;
    transaction.rollback().await?;
    assert_eq!(page.current_seq, durable);
    assert_eq!(page.events.iter().map(|event| event.batch.batch_id).collect::<Vec<_>>(), vec![2]);

    room_pool.close().await;
    control.close().await;
    Ok(())
}

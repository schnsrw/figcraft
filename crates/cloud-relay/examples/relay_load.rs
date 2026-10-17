//! Relay load harness (S5-024, S5-025): a real collaboration hub serving [`CloudBackend`] on
//! loopback against the dev PostgreSQL and S3, driven by simulated editors speaking the collab
//! protocol over raw WebSockets.
//!
//! Phases, in order: a steady run (N editors across M documents, each sending one property-change
//! batch every `1/rate` seconds and timing it until its confirmation, plus slow readers that join
//! and never read), a reconnect storm (K editors drop at once and reconnect together), a large
//! document (seeded with many nodes, then cold-opened and edited) and room recovery (cold opens of
//! documents with a snapshot of S nodes and E events after it, the events left by a relay that
//! died without snapshotting) and isolation (an editor commits steadily in one room while a large
//! document cold-opens in another: how long the first room's commits stall).
//!
//! It reports commit latency (batch sent to confirmation received), welcome and recovery times,
//! the process RSS per open document (clients run in the same process but keep no replicas),
//! PostgreSQL rows and bytes written to `document_events` per editor-hour, and outbound bytes.
//!
//! ```text
//! FIGCRAFT_TEST_DATABASE_URL_FILE=… FIGCRAFT_TEST_S3_DIR=… FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE=… \
//!   cargo run --release -p figcraft-cloud-relay --example relay_load -- \
//!   --editors 20 --documents 5 --seconds 120 --rate 2 --slow 2 --storm 20 \
//!   --large-nodes 20000 --recovery-nodes 20000 --recovery-events 500
//! ```
//!
//! `--phases steady,storm,large,recovery` picks phases, `--timeout S` sets the backend's storage
//! timeout (recovery included; 15 s like the relay) and `--cpu-nodes N` times the collaboration
//! core alone. Fixture rows use the f1 UUID prefix; every run makes new documents in that tenant. The admin
//! URL is optional: without it the database figures are skipped (row-level security hides the
//! byte sizes from the runtime role's catalog queries).

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use figcraft_cloud_domain::{CloudDocumentId, ProjectId, TenantId, WorkspaceId};
use figcraft_cloud_relay::collab_backend::{CloudBackend, CloudBackendConfig};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::metadata::create_document;
use figcraft_cloud_storage::objects::ObjectStoreSettings;
use figcraft_cloud_storage::share_links::{ShareRole, create_share_link};
use figcraft_collab::message::{self, CatchUp, ClientMsg, ServerMsg};
use figcraft_collab::{Change, Client, Json, NodeId, Prop};
use figcraft_collab_net::NetLimits;
use figcraft_collab_net::hub::Hosting;
use figcraft_doc::NodeKind;
use sqlx::postgres::{PgPool, PgPoolOptions};
use tungstenite::protocol::WebSocket;
use tungstenite::{Bytes, Message};

type Error = Box<dyn std::error::Error + Send + Sync>;
type R<T> = Result<T, Error>;

fn err(error: impl std::fmt::Debug) -> Error {
    format!("{error:?}").into()
}

const TENANT: &str = "f1000000-0000-4000-8000-000000000041";
const ACTOR: &str = "f1000000-0000-4000-8000-000000000049";
const WORKSPACE: &str = "f1100000-0000-4000-8000-000000000041";
const PROJECT: &str = "f1200000-0000-4000-8000-000000000041";
const SLUG: &str = "f1-relay-load";
/// Longest any one read waits.
const WAIT: Duration = Duration::from_secs(60);
/// Pause between a recovery writer's batches.
const WRITER_PACE: Duration = Duration::from_millis(20);
/// Creates per seeding batch.
const SEED_BATCH: usize = 2000;
/// Largest message a client decodes.
const MAX_DECODE: usize = 256 << 20;

#[derive(Clone, Debug)]
struct Options {
    editors: usize,
    documents: usize,
    seconds: u64,
    rate: f64,
    slow: usize,
    storm: usize,
    large_nodes: usize,
    large_commits: usize,
    recovery_nodes: usize,
    recovery_events: usize,
    recovery_runs: usize,
    cpu_nodes: usize,
    /// The backend's storage timeout (recovery included); the relay's default is 15 s.
    timeout: u64,
    /// Which of steady, storm, large and recovery run.
    phases: Vec<String>,
}

impl Options {
    fn parse() -> R<Self> {
        let mut options = Options {
            editors: 20,
            documents: 5,
            seconds: 120,
            rate: 2.0,
            slow: 2,
            storm: 20,
            large_nodes: 20_000,
            large_commits: 50,
            recovery_nodes: 20_000,
            recovery_events: 500,
            recovery_runs: 3,
            cpu_nodes: 0,
            timeout: 15,
            phases: PHASES.iter().map(|p| p.to_string()).collect(),
        };
        let args: Vec<String> = std::env::args().skip(1).collect();
        for pair in args.chunks(2) {
            let [flag, value] = pair else { return Err(format!("{pair:?}: every flag takes a value").into()) };
            let number = || value.parse::<usize>().map_err(err);
            match flag.as_str() {
                "--editors" => options.editors = number()?.clamp(1, 2000),
                "--documents" => options.documents = number()?.clamp(1, 500),
                "--seconds" => options.seconds = value.parse::<u64>().map_err(err)?.clamp(1, 86_400),
                "--rate" => options.rate = value.parse::<f64>().map_err(err)?.clamp(0.01, 50.0),
                "--slow" => options.slow = number()?.min(500),
                "--storm" => options.storm = number()?.min(2000),
                "--large-nodes" => options.large_nodes = number()?.min(2_000_000),
                "--large-commits" => options.large_commits = number()?.min(10_000),
                "--recovery-nodes" => options.recovery_nodes = number()?.min(2_000_000),
                "--recovery-events" => options.recovery_events = number()?.min(100_000),
                "--cpu-nodes" => options.cpu_nodes = number()?.min(2_000_000),
                "--recovery-runs" => options.recovery_runs = number()?.clamp(1, 20),
                "--timeout" => options.timeout = value.parse::<u64>().map_err(err)?.clamp(1, 600),
                "--phases" => {
                    options.phases = value.split(',').map(str::to_string).collect();
                    if let Some(bad) = options.phases.iter().find(|p| !PHASES.contains(&p.as_str())) {
                        return Err(format!("unknown phase {bad} (phases: {})", PHASES.join(",")).into());
                    }
                }
                other => return Err(format!("unknown flag {other}").into()),
            }
        }
        if !options.rate.is_finite() {
            return Err("--rate must be finite".into());
        }
        Ok(options)
    }

    fn runs(&self, phase: &str) -> bool {
        self.phases.iter().any(|p| p == phase)
    }
}

const PHASES: [&str; 5] = ["steady", "storm", "large", "recovery", "isolation"];

/// Relay warnings and errors to stderr, so a refused hello says why.
struct StderrLog;

impl log::Log for StderrLog {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!("[{}] {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

static LOGGER: StderrLog = StderrLog;

/// The dev services and the fixture tenant.
struct Env {
    runtime: tokio::runtime::Runtime,
    pool: PgPool,
    admin: Option<PgPool>,
    database_url: String,
    objects: ObjectStoreSettings,
    timeout: Duration,
}

fn read_trimmed(path: impl AsRef<std::path::Path>) -> R<String> {
    Ok(std::fs::read_to_string(path)?.trim_end().to_string())
}

impl Env {
    fn new(timeout: Duration) -> R<Self> {
        let database_url = read_trimmed(std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE").map_err(|_| "FIGCRAFT_TEST_DATABASE_URL_FILE is unset")?)?;
        let s3 = std::env::var("FIGCRAFT_TEST_S3_DIR").map_err(|_| "FIGCRAFT_TEST_S3_DIR is unset")?;
        let s3 = std::path::Path::new(&s3);
        let objects = ObjectStoreSettings {
            endpoint: read_trimmed(s3.join("s3_endpoint"))?,
            region: "us-east-1".into(),
            bucket: "figcraft".into(),
            access_key: read_trimmed(s3.join("s3_access_key"))?,
            secret_key: read_trimmed(s3.join("s3_secret_key"))?,
            path_style: true,
        };
        let admin_url = std::env::var("FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE").ok().map(read_trimmed).transpose()?;
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let (pool, admin) = runtime.block_on(async {
            let pool = PgPoolOptions::new().max_connections(2).connect(&database_url).await?;
            let admin = match admin_url {
                Some(url) => Some(PgPoolOptions::new().max_connections(1).connect(&url).await?),
                None => None,
            };
            let mut transaction = begin_tenant_transaction(&pool, TENANT.parse().map_err(err)?).await.map_err(err)?;
            let statements = [
                "INSERT INTO principals (id, kind, display_name) VALUES ($2::uuid, 'user', 'relay load') ON CONFLICT DO NOTHING",
                "INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $5, 'relay load', 'test', 'local') ON CONFLICT DO NOTHING",
                "INSERT INTO workspaces (id, tenant_id, name) VALUES ($3::uuid, $1::uuid, 'relay load') ON CONFLICT DO NOTHING",
                "INSERT INTO projects (id, tenant_id, workspace_id, name) VALUES ($4::uuid, $1::uuid, $3::uuid, 'relay load') ON CONFLICT DO NOTHING",
            ];
            for statement in statements {
                sqlx::query(statement)
                    .bind(TENANT)
                    .bind(ACTOR)
                    .bind(WORKSPACE)
                    .bind(PROJECT)
                    .bind(SLUG)
                    .execute(transaction.connection())
                    .await?;
            }
            transaction.commit().await?;
            Ok::<_, Error>((pool, admin))
        })?;
        Ok(Self { runtime, pool, admin, database_url, objects, timeout })
    }

    fn tenant() -> R<TenantId> {
        TENANT.parse().map_err(err)
    }

    /// A new document and an editor link to it: (file, token).
    fn document(&self) -> R<(CloudDocumentId, String)> {
        self.runtime.block_on(async {
            let mut transaction = begin_tenant_transaction(&self.pool, Self::tenant()?).await.map_err(err)?;
            let id = CloudDocumentId::random();
            let workspace: WorkspaceId = WORKSPACE.parse().map_err(err)?;
            let project: ProjectId = PROJECT.parse().map_err(err)?;
            create_document(&mut transaction, id, workspace, project, "relay load doc").await.map_err(err)?;
            let link = create_share_link(&mut transaction, id, ShareRole::Editor, None, ACTOR.parse().map_err(err)?, None).await.map_err(err)?;
            transaction.commit().await?;
            Ok((id, link.secret))
        })
    }

    fn serve(&self) -> R<Hosting> {
        let mut config = CloudBackendConfig::new(self.database_url.clone(), self.objects.clone());
        config.max_connections = 4;
        config.timeout = self.timeout;
        let backend = CloudBackend::connect(&config).map_err(err)?;
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))?;
        let limits = NetLimits { max_connections: 4096, hello_timeout: Duration::from_secs(30), ..NetLimits::default() };
        Ok(Hosting::serve(listener, backend, limits)?)
    }

    /// The tenant's `document_events` rows and their bytes, and its verified snapshot bytes, plus
    /// the table's total size and the database's WAL position (both shared with whatever else
    /// writes to the dev database).
    fn storage(&self) -> R<Option<Storage>> {
        let Some(admin) = &self.admin else { return Ok(None) };
        self.runtime.block_on(async {
            let row: (i64, i64, i64, i64, i64, i64) = sqlx::query_as(
                "SELECT (SELECT count(*) FROM document_events WHERE tenant_id = $1::uuid),
                        (SELECT coalesce(sum(pg_column_size(e.*)), 0)::bigint FROM document_events e WHERE tenant_id = $1::uuid),
                        (SELECT count(*) FROM document_snapshots WHERE tenant_id = $1::uuid),
                        (SELECT coalesce(sum(size_bytes), 0)::bigint FROM document_snapshots WHERE tenant_id = $1::uuid),
                        pg_total_relation_size('document_events'),
                        pg_wal_lsn_diff(pg_current_wal_lsn(), '0/0')::bigint",
            )
            .bind(TENANT)
            .fetch_one(admin)
            .await?;
            Ok(Some(Storage { events: row.0, event_bytes: row.1, snapshots: row.2, snapshot_bytes: row.3, table_bytes: row.4, wal: row.5 }))
        })
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Storage {
    events: i64,
    event_bytes: i64,
    snapshots: i64,
    snapshot_bytes: i64,
    table_bytes: i64,
    wal: i64,
}

impl Storage {
    fn minus(self, before: Storage) -> Storage {
        Storage {
            events: self.events - before.events,
            event_bytes: self.event_bytes - before.event_bytes,
            snapshots: self.snapshots - before.snapshots,
            snapshot_bytes: self.snapshot_bytes - before.snapshot_bytes,
            table_bytes: self.table_bytes - before.table_bytes,
            wal: self.wal - before.wal,
        }
    }
}

/// This process's resident set size in KiB (`ps`, which macOS, Linux and FreeBSD all have).
fn rss_kib() -> u64 {
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}

/// One simulated editor.
struct Editor {
    ws: WebSocket<TcpStream>,
    me: u32,
    seq: u64,
    page: NodeId,
    batch: u64,
    /// Bytes of messages received (the hub's outbound traffic to this client).
    received: u64,
    rejected: u64,
}

/// A joined editor and how long its welcome took, and how big it was.
struct Joined {
    editor: Editor,
    welcome: Duration,
    welcome_bytes: usize,
}

impl Editor {
    fn join(addr: SocketAddr, file: &str, token: &str, name: &str) -> R<Joined> {
        let started = Instant::now();
        let stream = TcpStream::connect(addr)?;
        stream.set_read_timeout(Some(WAIT))?;
        stream.set_nodelay(true)?;
        let (ws, _) = tungstenite::client::client(format!("ws://{addr}/ws/v1/collab"), stream).map_err(err)?;
        let mut editor = Editor { ws, me: 0, seq: 0, page: NodeId(0), batch: 0, received: 0, rejected: 0 };
        editor.send(&Client::hello(file, token, name, ""))?;
        loop {
            let bytes = editor.raw()?.ok_or("closed before the welcome")?;
            match message::decode::<ServerMsg>(&bytes, MAX_DECODE).map_err(err)? {
                ServerMsg::Welcome { client_id, seq, catch_up, .. } => {
                    let welcome = started.elapsed();
                    let page = match &catch_up {
                        CatchUp::Snapshot(changes) => changes.iter().find_map(|change| match change {
                            Change::Create { id, parent: None, .. } => Some(*id),
                            _ => None,
                        }),
                        _ => None,
                    }
                    .ok_or("the welcome had no page")?;
                    (editor.me, editor.seq, editor.page) = (client_id, seq, page);
                    return Ok(Joined { editor, welcome, welcome_bytes: bytes.len() });
                }
                ServerMsg::Refused { reason } => return Err(format!("refused: {reason}").into()),
                _ => {}
            }
        }
    }

    fn send(&mut self, msg: &ClientMsg) -> R<()> {
        self.ws.send(Message::Binary(Bytes::from(message::encode(msg).map_err(err)?))).map_err(err)
    }

    /// The next binary message's bytes; `None` once closed.
    fn raw(&mut self) -> R<Option<Bytes>> {
        loop {
            match self.ws.read() {
                Ok(Message::Binary(bytes)) => {
                    self.received += bytes.len() as u64;
                    return Ok(Some(bytes));
                }
                Ok(Message::Close(_)) => return Ok(None),
                Ok(_) => {}
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => return Ok(None),
                Err(error) => return Err(err(error)),
            }
        }
    }

    /// Send `changes` and read until they are confirmed: the time it took (`None`: refused).
    fn commit(&mut self, changes: Vec<Change>) -> R<Option<Duration>> {
        self.batch += 1;
        let batch_id = self.batch;
        let started = Instant::now();
        self.send(&ClientMsg::Changes { base_seq: self.seq, batch_id, changes })?;
        loop {
            let bytes = self.raw()?.ok_or("closed before the confirmation")?;
            match message::decode::<ServerMsg>(&bytes, MAX_DECODE).map_err(err)? {
                ServerMsg::Changes(accepted) => {
                    self.seq = self.seq.max(accepted.seq);
                    if accepted.author == self.me && accepted.batch_id == batch_id {
                        return Ok(Some(started.elapsed()));
                    }
                }
                ServerMsg::Reject { batch_id: refused, .. } if refused == batch_id => {
                    self.rejected += 1;
                    return Ok(None);
                }
                _ => {}
            }
        }
    }

    fn rename(&mut self, name: &str) -> R<Option<Duration>> {
        let changes = vec![Change::Set { id: self.page, prop: Prop::Name, value: Json::of(name).map_err(err)? }];
        self.commit(changes)
    }

    /// Create `count` frames on the page, `SEED_BATCH` per batch: each batch's commit time.
    fn seed(&mut self, count: usize) -> R<Vec<Duration>> {
        let kind = Json::of(&NodeKind::Frame).map_err(err)?;
        let positions = figcraft_collab::frac::n_between(None, None, count).map_err(err)?;
        let mut times = Vec::new();
        let mut next = 1u64;
        for chunk in positions.chunks(SEED_BATCH) {
            let changes = chunk
                .iter()
                .map(|position| {
                    let id = NodeId((u64::from(self.me) << 32) | next);
                    next += 1;
                    Change::Create { id, kind: kind.clone(), parent: Some(self.page), position: position.clone(), props: vec![] }
                })
                .collect();
            times.push(self.commit(changes)?.ok_or("a seeding batch was refused")?);
        }
        Ok(times)
    }
}

/// Percentiles of a sample, in milliseconds.
#[derive(Clone, Copy, Debug, Default)]
struct Summary {
    count: usize,
    p50: f64,
    p95: f64,
    p99: f64,
    max: f64,
}

fn summary(samples: &[Duration]) -> Summary {
    let mut ms: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    ms.sort_by(f64::total_cmp);
    let at = |q: f64| -> f64 {
        if ms.is_empty() {
            return 0.0;
        }
        let index = ((q * ms.len() as f64).ceil() as usize).saturating_sub(1).min(ms.len() - 1);
        ms.get(index).copied().unwrap_or(0.0)
    };
    Summary { count: ms.len(), p50: at(0.50), p95: at(0.95), p99: at(0.99), max: ms.last().copied().unwrap_or(0.0) }
}

fn show(label: &str, s: Summary) {
    println!("| {label} | {} | {:.1} | {:.1} | {:.1} | {:.1} |", s.count, s.p50, s.p95, s.p99, s.max);
}

/// What one editor thread measured.
struct Run {
    editor: Editor,
    latencies: Vec<Duration>,
}

/// Drive `editor` for `seconds`, one rename every `1/rate` seconds.
fn drive(mut editor: Editor, seconds: u64, rate: f64, offset: Duration) -> R<Run> {
    let period = Duration::from_secs_f64(1.0 / rate);
    let end = Instant::now() + Duration::from_secs(seconds);
    let mut next = Instant::now() + offset;
    let mut latencies = Vec::new();
    let mut n = 0u64;
    while next < end {
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        n += 1;
        if let Some(latency) = editor.rename(&format!("c{} n{n}", editor.me))? {
            latencies.push(latency);
        }
        next += period;
    }
    Ok(Run { editor, latencies })
}

/// The collaboration core alone, in this process (no network, no storage): how long a server of
/// `nodes` frames takes to capture its state, restore from it and welcome a client.
fn cpu(nodes: usize) -> R<()> {
    use figcraft_collab::{Document, Limits, Outgoing, Server};
    let doc = Document::new("Untitled");
    let page = doc.first_page().ok_or("no page")?;
    let mut server = Server::new(doc, "", Limits::default());
    let ClientMsg::Hello(hello) = Client::hello("cpu", "", "cpu", "") else { return Err("not a hello".into()) };
    let (me, _) = server.join(hello.clone()).map_err(err)?;
    let kind = Json::of(&NodeKind::Frame).map_err(err)?;
    let positions = figcraft_collab::frac::n_between(None, None, nodes).map_err(err)?;
    let mut next = 1u64;
    let started = Instant::now();
    for (batch, chunk) in positions.chunks(SEED_BATCH).enumerate() {
        let changes = chunk
            .iter()
            .map(|position| {
                let id = NodeId((u64::from(me) << 32) | next);
                next += 1;
                Change::Create { id, kind: kind.clone(), parent: Some(page), position: position.clone(), props: vec![] }
            })
            .collect();
        let out = server.receive(me, ClientMsg::Changes { base_seq: server.seq(), batch_id: batch as u64 + 1, changes });
        if out.iter().any(|o| matches!(o, Outgoing::To(_, ServerMsg::Reject { .. }))) {
            return Err("a cpu seeding batch was refused".into());
        }
    }
    let seeded = started.elapsed();
    let started = Instant::now();
    let state = server.state().map_err(err)?;
    let captured = started.elapsed();
    let started = Instant::now();
    let mut restored = Server::restore(state, "", Limits::default()).map_err(err)?;
    let restore = started.elapsed();
    let started = Instant::now();
    let (_, out) = restored.join(hello).map_err(err)?;
    let welcome = out.into_iter().find_map(|o| match o {
        Outgoing::To(_, msg) => Some(msg),
        _ => None,
    });
    let joined = started.elapsed();
    let started = Instant::now();
    let bytes = message::encode(&welcome.ok_or("no welcome")?).map_err(err)?;
    let encoded = started.elapsed();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    println!(
        "\n## collaboration core alone, {nodes} frames\n\nseed {:.0} ms ({} batches); state {:.0} ms; restore {:.0} ms; join {:.0} ms; encode welcome {:.0} ms ({} B)",
        ms(seeded),
        positions.len().div_ceil(SEED_BATCH),
        ms(captured),
        ms(restore),
        ms(joined),
        ms(encoded),
        bytes.len()
    );
    Ok(())
}

fn main() -> R<()> {
    let options = Options::parse()?;
    if options.cpu_nodes > 0 {
        return cpu(options.cpu_nodes);
    }
    // Only fails when a logger is already set, which nothing else here does.
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Warn);
    }
    let env = Env::new(Duration::from_secs(options.timeout))?;
    println!("# relay load: {options:?}");
    let rss_start = rss_kib();
    let mut hosting = env.serve()?;
    let addr = hosting.addr();
    let rss_hub = rss_kib();
    let documents: Vec<(CloudDocumentId, String)> = (0..options.documents).map(|_| env.document()).collect::<R<_>>()?;

    let editors = if options.runs("steady") {
        steady(&options, &env, &hosting, &documents, (rss_start, rss_hub))?
    } else if options.runs("storm") {
        let mut editors = Vec::new();
        for i in 0..options.storm.min(options.editors) {
            let (document, token) = documents.get(i % documents.len()).ok_or("no documents")?;
            editors.push(Editor::join(addr, &document.to_string(), token, &format!("editor {i}"))?.editor);
        }
        editors
    } else {
        Vec::new()
    };
    if options.runs("storm") {
        storm(&options, addr, &documents, editors)?;
    } else {
        drop(editors);
    }
    if options.runs("large") && options.large_nodes > 0 {
        large(&options, &env, addr)?;
    }
    hosting.stop();
    drop(hosting);
    if options.runs("isolation") && options.large_nodes > 0 {
        isolation(&options, &env)?;
    }
    if options.runs("recovery") {
        recovery(&options, &env)?;
    }
    println!("\nRSS at the end {} KiB", rss_kib());
    Ok(())
}

/// N editors across M documents for `--seconds`, plus slow readers. Returns the editors, still
/// connected. `rss` is the RSS at the start and once the hub was up.
fn steady(options: &Options, env: &Env, hosting: &Hosting, documents: &[(CloudDocumentId, String)], rss: (u64, u64)) -> R<Vec<Editor>> {
    let (rss_start, rss_hub) = rss;
    let addr = hosting.addr();
    let storage_before = env.storage()?;
    let mut joined = Vec::new();
    let mut welcomes = Vec::new();
    for i in 0..options.editors {
        let (document, token) = documents.get(i % documents.len()).ok_or("no documents")?;
        let join = Editor::join(addr, &document.to_string(), token, &format!("editor {i}"))?;
        welcomes.push(join.welcome);
        joined.push(join.editor);
    }
    let rss_open = rss_kib();
    let mut slow = Vec::new();
    for i in 0..options.slow {
        let (document, token) = documents.get(i % documents.len()).ok_or("no documents")?;
        slow.push(Editor::join(addr, &document.to_string(), token, &format!("slow {i}"))?.editor);
    }
    println!("connections after joining: {}", hosting.connections());
    let period = 1.0 / options.rate;
    let count = joined.len().max(1) as f64;
    let threads: Vec<_> = joined
        .into_iter()
        .enumerate()
        .map(|(i, editor)| {
            let offset = Duration::from_secs_f64(period * i as f64 / count);
            let (seconds, rate) = (options.seconds, options.rate);
            std::thread::spawn(move || drive(editor, seconds, rate, offset))
        })
        .collect();
    let started = Instant::now();
    let mut samples = Vec::new();
    while threads.iter().any(|t| !t.is_finished()) {
        std::thread::sleep(Duration::from_secs(5));
        samples.push((started.elapsed().as_secs(), hosting.connections(), hosting.queued_bytes(), rss_kib()));
    }
    let wall = started.elapsed();
    let mut latencies = Vec::new();
    let mut editors = Vec::new();
    let mut failures = 0usize;
    for thread in threads {
        match thread.join() {
            Ok(Ok(run)) => {
                latencies.extend(run.latencies);
                editors.push(run.editor);
            }
            Ok(Err(error)) => {
                failures += 1;
                eprintln!("an editor failed: {error}");
            }
            Err(_) => failures += 1,
        }
    }
    let storage_after = env.storage()?;
    let received: u64 = editors.iter().map(|e| e.received).sum();
    let rejected: u64 = editors.iter().map(|e| e.rejected).sum();
    let editor_seconds = options.editors as f64 * wall.as_secs_f64();
    let editor_hours = editor_seconds / 3600.0;

    println!(
        "\n## steady: {} editors, {} documents, {:.0} s, {} batches/s each, {} slow readers",
        options.editors,
        options.documents,
        wall.as_secs_f64(),
        options.rate,
        options.slow
    );
    println!("\n| measure | n | p50 ms | p95 ms | p99 ms | max ms |\n|---|---|---|---|---|---|");
    show("commit latency (send to confirmation)", summary(&latencies));
    show("welcome (connect to welcome, first joins)", summary(&welcomes));
    println!("\nfailed editors {failures}; refused batches {rejected}; confirmed {}", latencies.len());
    println!("commits/s {:.1}", latencies.len() as f64 / wall.as_secs_f64());
    println!(
        "outbound bytes to editors {received} ({:.0} B/s per editor, {:.1} MiB per editor-hour)",
        received as f64 / editor_seconds,
        received as f64 / editor_hours / 1048576.0
    );
    println!(
        "RSS KiB: start {rss_start}, hub up {rss_hub}, {} documents open {rss_open} ({:.0} KiB per open document incl. {} client threads)",
        options.documents,
        (rss_open.saturating_sub(rss_hub)) as f64 / options.documents as f64,
        options.editors
    );
    println!("samples (s, connections, queued bytes, RSS KiB): {samples:?}");
    if let (Some(before), Some(after)) = (storage_before, storage_after) {
        let delta = after.minus(before);
        println!(
            "document_events: +{} rows, +{} bytes in rows ({:.0} rows/s, {:.0} B/row; {:.0} rows and {:.2} MiB per editor-hour); snapshots +{} ({} B)",
            delta.events,
            delta.event_bytes,
            delta.events as f64 / wall.as_secs_f64(),
            delta.event_bytes as f64 / delta.events.max(1) as f64,
            delta.events as f64 / editor_hours,
            delta.event_bytes as f64 / editor_hours / 1048576.0,
            delta.snapshots,
            delta.snapshot_bytes
        );
        println!(
            "shared database (other writers included): table +{} B (pages), WAL +{} B ({:.0} B/s)",
            delta.table_bytes,
            delta.wal,
            delta.wal as f64 / wall.as_secs_f64()
        );
    }
    // Slow readers: still connected, and the bytes queued for them.
    println!("slow readers: connections now {}, queued bytes {}", hosting.connections(), hosting.queued_bytes());
    drop(slow);
    Ok(editors)
}

/// Drop `editors` at once and reconnect `--storm` of them together behind a barrier.
fn storm(options: &Options, addr: SocketAddr, documents: &[(CloudDocumentId, String)], mut editors: Vec<Editor>) -> R<()> {
    let storm = options.storm.min(editors.len());
    let mut storm_tokens = Vec::new();
    for i in 0..storm {
        let (document, token) = documents.get(i % documents.len()).ok_or("no documents")?;
        storm_tokens.push((document.to_string(), token.clone()));
    }
    let keep = editors.split_off(storm);
    drop(editors);
    let barrier = Arc::new(Barrier::new(storm.max(1)));
    let dropped_at = Instant::now();
    let storm_threads: Vec<_> = storm_tokens
        .into_iter()
        .enumerate()
        .map(|(i, (file, token))| {
            let barrier = barrier.clone();
            std::thread::spawn(move || -> R<(Duration, Duration)> {
                barrier.wait();
                let mut join = Editor::join(addr, &file, &token, &format!("storm {i}"))?;
                let first = join.editor.rename("after the storm")?.ok_or("refused after the storm")?;
                Ok((join.welcome, first))
            })
        })
        .collect();
    let (mut storm_welcomes, mut storm_firsts, mut storm_failures) = (Vec::new(), Vec::new(), 0usize);
    for thread in storm_threads {
        match thread.join() {
            Ok(Ok((welcome, first))) => {
                storm_welcomes.push(welcome);
                storm_firsts.push(first);
            }
            Ok(Err(error)) => {
                storm_failures += 1;
                eprintln!("a storm reconnect failed: {error}");
            }
            Err(_) => storm_failures += 1,
        }
    }
    let storm_all = dropped_at.elapsed();
    drop(keep);
    println!("\n## reconnect storm: {storm} editors dropped at once and reconnected together");
    println!("\n| measure | n | p50 ms | p95 ms | p99 ms | max ms |\n|---|---|---|---|---|---|");
    show("reconnect to welcome", summary(&storm_welcomes));
    show("first commit after reconnect", summary(&storm_firsts));
    println!("\nstorm failures {storm_failures}; all back and committed in {:.0} ms", storm_all.as_secs_f64() * 1000.0);
    Ok(())
}

/// Seed a document with `--large-nodes` frames, leave (the room commits and snapshots), then
/// cold-open it, edit it and join it warm. A refused cold open is reported, not fatal.
fn large(options: &Options, env: &Env, addr: SocketAddr) -> R<()> {
    let (document, token) = env.document()?;
    let file = document.to_string();
    let mut seeder = Editor::join(addr, &file, &token, "seeder")?.editor;
    let seeding = summary(&seeder.seed(options.large_nodes)?);
    drop(seeder);
    println!("\n## large document: {} frames", options.large_nodes);
    println!("\n| measure | n | p50 ms | p95 ms | p99 ms | max ms |\n|---|---|---|---|---|---|");
    show(&format!("seeding commit ({SEED_BATCH} creates per batch)"), seeding);
    // The room closes (commit and snapshot) once the last client left.
    std::thread::sleep(Duration::from_secs(2));
    let rss_before = rss_kib();
    let started = Instant::now();
    let opened = match Editor::join(addr, &file, &token, "large") {
        Ok(opened) => opened,
        Err(error) => {
            println!("\ncold open failed after {:.0} ms: {error}", started.elapsed().as_secs_f64() * 1000.0);
            return Ok(());
        }
    };
    let rss_after = rss_kib();
    let mut editor = opened.editor;
    let mut large = Vec::new();
    for n in 0..options.large_commits {
        if let Some(latency) = editor.rename(&format!("large {n}"))? {
            large.push(latency);
        }
    }
    let second = Editor::join(addr, &file, &token, "large 2")?;
    show("rename commit on the large document", summary(&large));
    println!(
        "\ncold open (recover + welcome) {:.0} ms, welcome {} B; warm join {:.0} ms; RSS {rss_before} -> {rss_after} KiB (+{} KiB)",
        opened.welcome.as_secs_f64() * 1000.0,
        opened.welcome_bytes,
        second.welcome.as_secs_f64() * 1000.0,
        rss_after.saturating_sub(rss_before)
    );
    Ok(())
}

/// One room commits every `ISOLATION_PACE` while another room cold-opens a large document: the
/// first room's commit latencies during the open show whether a slow restore stops other rooms.
fn isolation(options: &Options, env: &Env) -> R<()> {
    const ISOLATION_PACE: Duration = Duration::from_millis(20);
    let (large, large_token) = env.document()?;
    let (small, small_token) = env.document()?;
    // Seed the large document, then close its room (commit and snapshot) so the next open is cold.
    let mut seeding = env.serve()?;
    let mut seeder = Editor::join(seeding.addr(), &large.to_string(), &large_token, "seeder")?.editor;
    seeder.seed(options.large_nodes)?;
    drop(seeder);
    seeding.stop();

    let mut hosting = env.serve()?;
    let addr = hosting.addr();
    let mut steady = Editor::join(addr, &small.to_string(), &small_token, "steady")?.editor;
    // Warm: a few commits before the cold open starts.
    for n in 0..20 {
        steady.rename(&format!("warm {n}"))?.ok_or("a warm-up commit was refused")?;
    }
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = {
        let stop = stop.clone();
        std::thread::spawn(move || -> R<Vec<(Instant, Duration)>> {
            let mut samples = Vec::new();
            let mut n = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let at = Instant::now();
                if let Some(latency) = steady.rename(&format!("during {n}"))? {
                    samples.push((at, latency));
                }
                n += 1;
                std::thread::sleep(ISOLATION_PACE);
            }
            Ok(samples)
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    let opening_from = Instant::now();
    let opened = Editor::join(addr, &large.to_string(), &large_token, "large");
    let opening_until = Instant::now();
    std::thread::sleep(Duration::from_millis(300));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let samples = worker.join().map_err(|_| "the steady editor panicked")??;
    let during: Vec<Duration> = samples.iter().filter(|(at, _)| *at >= opening_from && *at <= opening_until).map(|(_, l)| *l).collect();
    let before: Vec<Duration> = samples.iter().filter(|(at, _)| *at < opening_from).map(|(_, l)| *l).collect();
    println!(
        "\n## isolation: a {}-frame document cold-opens while another room commits every {} ms",
        options.large_nodes,
        ISOLATION_PACE.as_millis()
    );
    match &opened {
        Ok(joined) => println!("\ncold open (hello to welcome): {:.0} ms", joined.welcome.as_secs_f64() * 1000.0),
        Err(error) => println!("\ncold open failed: {error}"),
    }
    println!("\n| the steady room's commits | n | p50 ms | p95 ms | p99 ms | max ms |\n|---|---|---|---|---|---|");
    show("before the cold open", summary(&before));
    show("during the cold open", summary(&during));
    hosting.stop();
    Ok(())
}

/// Cold opens of documents with an S-node snapshot plus E events left by a relay that died
/// before snapshotting. A refused open is counted, not fatal.
fn recovery(options: &Options, env: &Env) -> R<()> {
    let scenarios = [(0, options.recovery_events), (options.recovery_nodes, 0), (options.recovery_nodes, options.recovery_events)];
    println!("\n## room recovery (cold open of a document: hello to welcome, {} runs each)", options.recovery_runs);
    println!("\n| snapshot nodes | events after it | n | refused | p50 ms | p95 ms | p99 ms | max ms |\n|---|---|---|---|---|---|---|---|");
    // The relays that "died": kept alive (unreachable) until the end so they never snapshot.
    let mut dead: Vec<(Hosting, Editor)> = Vec::new();
    for (nodes, events) in scenarios {
        let mut opens = Vec::new();
        let mut refused = 0usize;
        for _ in 0..options.recovery_runs {
            let (document, token) = env.document()?;
            let file = document.to_string();
            if nodes > 0 {
                let mut seeding = env.serve()?;
                let mut seeder = Editor::join(seeding.addr(), &file, &token, "seeder")?.editor;
                seeder.seed(nodes)?;
                drop(seeder);
                // Stopping closes the room: it commits and stores the snapshot.
                seeding.stop();
            }
            if events > 0 {
                let crashing = env.serve()?;
                let mut writer = match Editor::join(crashing.addr(), &file, &token, "writer") {
                    Ok(joined) => joined.editor,
                    Err(error) => {
                        eprintln!("the writer couldn't open the seeded document: {error}");
                        refused += 1;
                        continue;
                    }
                };
                for n in 0..events {
                    writer.rename(&format!("event {n}"))?;
                    // Under the hub's per-client message rate (NetLimits::rate, 100/s).
                    std::thread::sleep(WRITER_PACE);
                }
                dead.push((crashing, writer));
            }
            let mut fresh = env.serve()?;
            match Editor::join(fresh.addr(), &file, &token, "recovering") {
                Ok(opened) => opens.push(opened.welcome),
                Err(error) => {
                    eprintln!("a cold open failed: {error}");
                    refused += 1;
                }
            }
            fresh.stop();
        }
        let s = summary(&opens);
        println!("| {nodes} | {events} | {} | {refused} | {:.1} | {:.1} | {:.1} | {:.1} |", s.count, s.p50, s.p95, s.p99, s.max);
    }
    // The dead relays' rooms hold stale fencing tokens: stopping them now commits nothing new.
    for (mut hosting, editor) in dead {
        drop(editor);
        hosting.stop();
    }
    Ok(())
}

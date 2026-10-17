//! S6-014 webhook delivery against PostgreSQL (FIGCRAFT_TEST_DATABASE_URL_FILE) and a local HTTP
//! receiver. Tenants are random. Tests that claim work globally (`claim_due`) are serialized, and
//! only ever process their own tenant's deliveries.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use figcraft_cloud_domain::{TenantId, Uuid};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::host_systems::{HostRole, NewHostSystem, register_host_system, set_host_enabled};
use figcraft_cloud_storage::secret_box::SecretKey;
use figcraft_cloud_storage::webhook_delivery::{
    DEFAULT_MAX_ATTEMPTS, DeliveryState, Outcome, RETRY_DELAYS_SECS, SIGNATURE_TOLERANCE_SECS, claim_due, enqueue_delivery, get_delivery,
    list_attempts, list_deliveries, process_claimed, process_claimed_with, replay_delivery, verify_signature,
};
use figcraft_cloud_storage::webhooks::{DestinationPolicy, create_webhook_endpoint, rotate_webhook_secret, update_webhook_endpoint};
use sqlx::postgres::{PgPool, PgPoolOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const DEV: DestinationPolicy = DestinationPolicy { allow_private_networks: true };

fn serial() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn key() -> TestResult<SecretKey> {
    Ok(SecretKey::derive(b"s6-delivery-test-signing-key-01").map_err(|e| format!("{e:?}"))?)
}

async fn pool() -> TestResult<Option<PgPool>> {
    let Ok(file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; webhook delivery probe skipped");
        return Ok(None);
    };
    let url = std::fs::read_to_string(file)?;
    Ok(Some(PgPoolOptions::new().max_connections(8).connect(url.trim_end()).await?))
}

// ---- the receiver ----

#[derive(Clone, Debug)]
struct Received {
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Received {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }
}

#[derive(Clone, Copy, Debug)]
enum Reply {
    Status(u16),
    /// Take this long, then answer 200.
    Slow(Duration),
}

struct Receiver {
    port: u16,
    received: Arc<Mutex<Vec<Received>>>,
}

impl Receiver {
    async fn start(script: &[Reply]) -> TestResult<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let received = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(Mutex::new(script.iter().copied().collect::<VecDeque<_>>()));
        let (log, queue) = (Arc::clone(&received), Arc::clone(&script));
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let (log, queue) = (Arc::clone(&log), Arc::clone(&queue));
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let (head_end, length) = loop {
                        let Ok(read) = stream.read(&mut chunk).await else { return };
                        if read == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buffer[..end]).to_lowercase();
                            let length = head
                                .lines()
                                .find_map(|line| line.strip_prefix("content-length:").and_then(|value| value.trim().parse::<usize>().ok()))
                                .unwrap_or(0);
                            break (end + 4, length);
                        }
                    };
                    while buffer.len() < head_end + length {
                        let Ok(read) = stream.read(&mut chunk).await else { return };
                        if read == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                    }
                    let head = String::from_utf8_lossy(&buffer[..head_end - 4]).to_string();
                    let headers = head
                        .lines()
                        .skip(1)
                        .filter_map(|line| line.split_once(':').map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_owned())))
                        .collect();
                    if let Ok(mut log) = log.lock() {
                        log.push(Received { headers, body: buffer[head_end..head_end + length].to_vec() });
                    }
                    let reply = queue.lock().ok().and_then(|mut queue| queue.pop_front()).unwrap_or(Reply::Status(200));
                    let status = match reply {
                        Reply::Status(status) => status,
                        Reply::Slow(delay) => {
                            tokio::time::sleep(delay).await;
                            200
                        }
                    };
                    let response = format!("HTTP/1.1 {status} X\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok");
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Ok(Self { port, received })
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/hook", self.port)
    }

    fn requests(&self) -> Vec<Received> {
        self.received.lock().map(|log| log.clone()).unwrap_or_default()
    }
}

// ---- fixtures ----

struct Setup {
    tenant: TenantId,
    endpoint: Uuid,
    secret: String,
}

async fn setup(pool: &PgPool, url: &str) -> TestResult<Setup> {
    let tenant = TenantId::random();
    let mut tx = begin_tenant_transaction(pool, tenant).await.map_err(|e| format!("{e:?}"))?;
    sqlx::query("INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $2, 'delivery probe', 'test', 'local')")
        .bind(tenant.to_string())
        .bind(format!("wh-{}", tenant.as_uuid().simple()))
        .execute(tx.connection())
        .await?;
    register_host_system(
        &mut tx,
        &NewHostSystem {
            system_id: "host-a".into(),
            issuer: format!("https://wh-{}.example", Uuid::new_v4().simple()),
            audiences: vec!["figcraft".into()],
            allowed_origins: vec![],
            max_role: HostRole::Editor,
            allow_insecure_localhost: false,
        },
    )
    .await
    .map_err(|e| format!("{e:?}"))?;
    let (endpoint, secret) = create_webhook_endpoint(&mut tx, &key()?, DEV, "host-a", url, &[]).await.map_err(|e| format!("{e:?}"))?;
    tx.commit().await?;
    Ok(Setup { tenant, endpoint: endpoint.id, secret: secret.expose().to_owned() })
}

async fn enqueue(pool: &PgPool, setup: &Setup, event_id: Uuid, max_attempts: i32) -> TestResult<Uuid> {
    let body = format!("{{\"id\":\"{event_id}\",\"type\":\"document.created\",\"data\":{{\"document_id\":\"d\"}}}}");
    let mut tx = begin_tenant_transaction(pool, setup.tenant).await.map_err(|e| format!("{e:?}"))?;
    let id =
        enqueue_delivery(&mut tx, setup.endpoint, event_id, "document.created", body.as_bytes(), max_attempts).await.map_err(|e| format!("{e:?}"))?;
    tx.commit().await?;
    Ok(id.ok_or("duplicate")?)
}

/// What `claim_due` does for one delivery, without touching other tenants' work.
async fn lease(pool: &PgPool, setup: &Setup, delivery: Uuid) -> TestResult {
    let mut tx = begin_tenant_transaction(pool, setup.tenant).await.map_err(|e| format!("{e:?}"))?;
    sqlx::query(
        "UPDATE webhook_deliveries SET attempts = attempts + 1, lease_expires_at = clock_timestamp() + interval '60 seconds' WHERE id = $1::uuid",
    )
    .bind(delivery.to_string())
    .execute(tx.connection())
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn attempt(pool: &PgPool, setup: &Setup, delivery: Uuid) -> TestResult<Outcome> {
    lease(pool, setup, delivery).await?;
    Ok(process_claimed(pool, &key()?, DEV, setup.tenant, delivery).await.map_err(|e| format!("{e:?}"))?)
}

async fn record(pool: &PgPool, setup: &Setup, delivery: Uuid) -> TestResult<figcraft_cloud_storage::webhook_delivery::DeliveryRecord> {
    let mut tx = begin_tenant_transaction(pool, setup.tenant).await.map_err(|e| format!("{e:?}"))?;
    let found = get_delivery(&mut tx, "host-a", setup.endpoint, delivery).await.map_err(|e| format!("{e:?}"))?;
    tx.rollback().await?;
    Ok(found.ok_or("delivery")?)
}

async fn make_due(pool: &PgPool, setup: &Setup, delivery: Uuid) -> TestResult {
    let mut tx = begin_tenant_transaction(pool, setup.tenant).await.map_err(|e| format!("{e:?}"))?;
    sqlx::query(
        "UPDATE webhook_deliveries SET next_attempt_at = clock_timestamp() - interval '1 second', lease_expires_at = NULL WHERE id = $1::uuid",
    )
    .bind(delivery.to_string())
    .execute(tx.connection())
    .await?;
    tx.commit().await?;
    Ok(())
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| i64::try_from(d.as_secs()).unwrap_or(0)).unwrap_or(0)
}

// ---- tests ----

#[tokio::test]
async fn a_delivery_is_signed_with_the_endpoint_secret_and_carries_the_event_identity() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let receiver = Receiver::start(&[]).await?;
    let setup = setup(&pool, &receiver.url()).await?;
    let event = Uuid::new_v4();
    let delivery = enqueue(&pool, &setup, event, DEFAULT_MAX_ATTEMPTS).await?;
    assert_eq!(attempt(&pool, &setup, delivery).await?, Outcome::Delivered { status: 200 });

    let requests = receiver.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.header("x-figcraft-event-id"), Some(event.to_string().as_str()));
    assert_eq!(request.header("x-figcraft-event-type"), Some("document.created"));
    assert_eq!(request.header("x-figcraft-delivery-id"), Some(delivery.to_string().as_str()));
    assert_eq!(request.header("content-type"), Some("application/json"));
    let timestamp: i64 = request.header("x-figcraft-timestamp").ok_or("timestamp")?.parse()?;
    assert!((now() - timestamp).abs() <= 5);
    let signature = request.header("x-figcraft-signature").ok_or("signature")?;
    assert!(verify_signature(setup.secret.as_bytes(), signature, timestamp, &request.body, now(), SIGNATURE_TOLERANCE_SECS));
    // The signature binds the timestamp and the body, and no other secret verifies it.
    assert!(!verify_signature(setup.secret.as_bytes(), signature, timestamp + 1, &request.body, now(), SIGNATURE_TOLERANCE_SECS));
    assert!(!verify_signature(setup.secret.as_bytes(), signature, timestamp, b"{}", now(), SIGNATURE_TOLERANCE_SECS));
    assert!(!verify_signature(b"whsec_wrong", signature, timestamp, &request.body, now(), SIGNATURE_TOLERANCE_SECS));
    // A replayed capture is outside the receiver's window once it is old.
    assert!(!verify_signature(
        setup.secret.as_bytes(),
        signature,
        timestamp,
        &request.body,
        timestamp + SIGNATURE_TOLERANCE_SECS + 1,
        SIGNATURE_TOLERANCE_SECS
    ));
    assert!(!String::from_utf8_lossy(&request.body).contains("whsec_"));

    let done = record(&pool, &setup, delivery).await?;
    assert_eq!((done.state, done.attempts, done.last_status), (DeliveryState::Succeeded, 1, Some(200)));
    assert!(done.completed_at.is_some());
    // Publishing the same event to the same endpoint again is a no-op.
    let mut tx = begin_tenant_transaction(&pool, setup.tenant).await.map_err(|e| format!("{e:?}"))?;
    assert_eq!(enqueue_delivery(&mut tx, setup.endpoint, event, "document.created", b"{}", 8).await.map_err(|e| format!("{e:?}"))?, None);
    tx.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn failures_retry_on_the_schedule_then_succeed_with_the_same_event_id() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let receiver = Receiver::start(&[Reply::Status(500), Reply::Status(503), Reply::Status(302)]).await?;
    let setup = setup(&pool, &receiver.url()).await?;
    let event = Uuid::new_v4();
    let delivery = enqueue(&pool, &setup, event, DEFAULT_MAX_ATTEMPTS).await?;

    for (index, expected_status) in [500u16, 503, 302].into_iter().enumerate() {
        let outcome = attempt(&pool, &setup, delivery).await?;
        assert_eq!(outcome, Outcome::Retry { error: "http_status", status: Some(expected_status) }, "attempt {}", index + 1);
        let state = record(&pool, &setup, delivery).await?;
        assert_eq!((state.state, state.attempts), (DeliveryState::Pending, i32::try_from(index + 1)?));
        // The next attempt is due after the documented delay for this attempt number.
        let wait = state.next_attempt_at - now();
        let planned = RETRY_DELAYS_SECS[index];
        assert!((planned - 3..=planned + 3).contains(&wait), "attempt {}: wait {wait}s, planned {planned}s", index + 1);
        make_due(&pool, &setup, delivery).await?;
    }
    assert_eq!(attempt(&pool, &setup, delivery).await?, Outcome::Delivered { status: 200 });
    let done = record(&pool, &setup, delivery).await?;
    assert_eq!((done.state, done.attempts), (DeliveryState::Succeeded, 4));

    // Every attempt carried the same event id, delivery id and body; the receiver can deduplicate.
    let requests = receiver.requests();
    assert_eq!(requests.len(), 4);
    assert!(requests.iter().all(|r| r.header("x-figcraft-event-id") == Some(event.to_string().as_str())));
    assert!(requests.iter().all(|r| r.header("x-figcraft-delivery-id") == Some(delivery.to_string().as_str())));
    assert!(requests.windows(2).all(|pair| pair[0].body == pair[1].body));

    // The attempt log keeps status and timing, newest first, no bodies.
    let mut tx = begin_tenant_transaction(&pool, setup.tenant).await.map_err(|e| format!("{e:?}"))?;
    let attempts = list_attempts(&mut tx, delivery).await.map_err(|e| format!("{e:?}"))?;
    tx.rollback().await?;
    assert_eq!(
        attempts.iter().map(|a| (a.attempt, a.http_status)).collect::<Vec<_>>(),
        vec![(4, Some(200)), (3, Some(302)), (2, Some(503)), (1, Some(500))]
    );
    Ok(())
}

#[tokio::test]
async fn exhausted_deliveries_fail_terminally_and_can_be_replayed_by_hand() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let receiver = Receiver::start(&[Reply::Status(500), Reply::Status(500)]).await?;
    let setup = setup(&pool, &receiver.url()).await?;
    let event = Uuid::new_v4();
    let delivery = enqueue(&pool, &setup, event, 2).await?;
    assert!(matches!(attempt(&pool, &setup, delivery).await?, Outcome::Retry { .. }));
    make_due(&pool, &setup, delivery).await?;
    assert_eq!(attempt(&pool, &setup, delivery).await?, Outcome::Failed { error: "http_status", status: Some(500) });
    let failed = record(&pool, &setup, delivery).await?;
    assert_eq!((failed.state, failed.attempts, failed.last_error.as_deref()), (DeliveryState::Failed, 2, Some("http_status")));
    // Terminal: nothing is due, and processing it again does nothing.
    make_due(&pool, &setup, delivery).await?;
    assert_eq!(process_claimed(&pool, &key()?, DEV, setup.tenant, delivery).await.map_err(|e| format!("{e:?}"))?, Outcome::Skipped);
    assert_eq!(receiver.requests().len(), 2);

    // Manual replay: same delivery, same event id and body, a fresh budget.
    let before = receiver.requests()[0].clone();
    let mut tx = begin_tenant_transaction(&pool, setup.tenant).await.map_err(|e| format!("{e:?}"))?;
    assert!(replay_delivery(&mut tx, "host-a", setup.endpoint, delivery).await.map_err(|e| format!("{e:?}"))?);
    // A pending delivery cannot be replayed again, and a stranger's id is not found.
    assert!(!replay_delivery(&mut tx, "host-a", setup.endpoint, delivery).await.map_err(|e| format!("{e:?}"))?);
    assert!(replay_delivery(&mut tx, "host-a", setup.endpoint, Uuid::new_v4()).await.is_err());
    assert!(replay_delivery(&mut tx, "other-host", setup.endpoint, delivery).await.is_err());
    tx.commit().await?;
    let replayed = record(&pool, &setup, delivery).await?;
    assert_eq!((replayed.state, replayed.attempts, replayed.last_error), (DeliveryState::Pending, 0, None));
    assert_eq!(attempt(&pool, &setup, delivery).await?, Outcome::Delivered { status: 200 });
    let after = receiver.requests()[2].clone();
    assert_eq!(after.header("x-figcraft-event-id"), before.header("x-figcraft-event-id"));
    assert_eq!(after.body, before.body);
    // A finished delivery can be replayed too (a receiver that lost the event).
    let mut tx = begin_tenant_transaction(&pool, setup.tenant).await.map_err(|e| format!("{e:?}"))?;
    assert!(replay_delivery(&mut tx, "host-a", setup.endpoint, delivery).await.map_err(|e| format!("{e:?}"))?);
    tx.commit().await?;
    assert_eq!(attempt(&pool, &setup, delivery).await?, Outcome::Delivered { status: 200 });
    let all = receiver.requests();
    assert_eq!(all.len(), 4);
    assert!(all.iter().all(|r| r.header("x-figcraft-event-id") == before.header("x-figcraft-event-id")));
    let mut tx = begin_tenant_transaction(&pool, setup.tenant).await.map_err(|e| format!("{e:?}"))?;
    let listed = list_deliveries(&mut tx, "host-a", setup.endpoint, None).await.map_err(|e| format!("{e:?}"))?;
    let failed_only = list_deliveries(&mut tx, "host-a", setup.endpoint, Some(DeliveryState::Failed)).await.map_err(|e| format!("{e:?}"))?;
    tx.rollback().await?;
    assert_eq!((listed.len(), failed_only.len()), (1, 0));
    Ok(())
}

#[tokio::test]
async fn a_slow_receiver_times_out_and_is_retried() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let receiver = Receiver::start(&[Reply::Slow(Duration::from_secs(3))]).await?;
    let setup = setup(&pool, &receiver.url()).await?;
    let delivery = enqueue(&pool, &setup, Uuid::new_v4(), DEFAULT_MAX_ATTEMPTS).await?;
    lease(&pool, &setup, delivery).await?;
    let started = std::time::Instant::now();
    let outcome =
        process_claimed_with(&pool, &key()?, DEV, setup.tenant, delivery, Duration::from_millis(400)).await.map_err(|e| format!("{e:?}"))?;
    assert_eq!(outcome, Outcome::Retry { error: "timeout", status: None });
    assert!(started.elapsed() < Duration::from_secs(2));
    let state = record(&pool, &setup, delivery).await?;
    assert_eq!((state.state, state.last_error.as_deref(), state.last_status), (DeliveryState::Pending, Some("timeout"), None));
    // The next try, against a healthy receiver, succeeds.
    make_due(&pool, &setup, delivery).await?;
    assert_eq!(attempt(&pool, &setup, delivery).await?, Outcome::Delivered { status: 200 });
    Ok(())
}

#[tokio::test]
async fn unreachable_and_forbidden_destinations_are_failures_not_requests() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    // A port nothing listens on.
    let closed = TcpListener::bind("127.0.0.1:0").await?;
    let port = closed.local_addr()?.port();
    drop(closed);
    let setup_closed = setup(&pool, &format!("http://127.0.0.1:{port}/hook")).await?;
    let delivery = enqueue(&pool, &setup_closed, Uuid::new_v4(), DEFAULT_MAX_ATTEMPTS).await?;
    assert_eq!(attempt(&pool, &setup_closed, delivery).await?, Outcome::Retry { error: "connect_failure", status: None });

    // Production policy: loopback and private destinations are never contacted, whatever the
    // stored URL says (it could have been registered under the development policy).
    let receiver = Receiver::start(&[]).await?;
    let setup = setup(&pool, &receiver.url()).await?;
    let delivery = enqueue(&pool, &setup, Uuid::new_v4(), DEFAULT_MAX_ATTEMPTS).await?;
    lease(&pool, &setup, delivery).await?;
    let outcome = process_claimed(&pool, &key()?, DestinationPolicy::PUBLIC_ONLY, setup.tenant, delivery).await.map_err(|e| format!("{e:?}"))?;
    assert_eq!(outcome, Outcome::Retry { error: "blocked_destination", status: None });
    assert!(receiver.requests().is_empty(), "no request may reach a blocked destination");
    // A hostname that resolves to loopback is blocked the same way.
    let mut tx = begin_tenant_transaction(&pool, setup.tenant).await.map_err(|e| format!("{e:?}"))?;
    update_webhook_endpoint(&mut tx, DEV, "host-a", setup.endpoint, Some(&format!("http://localhost:{}/hook", receiver.port)), None, None)
        .await
        .map_err(|e| format!("{e:?}"))?;
    tx.commit().await?;
    make_due(&pool, &setup, delivery).await?;
    lease(&pool, &setup, delivery).await?;
    let outcome = process_claimed(&pool, &key()?, DestinationPolicy::PUBLIC_ONLY, setup.tenant, delivery).await.map_err(|e| format!("{e:?}"))?;
    assert_eq!(outcome, Outcome::Retry { error: "blocked_destination", status: None });
    assert!(receiver.requests().is_empty());
    Ok(())
}

#[tokio::test]
async fn published_events_reach_only_subscribed_endpoints_with_a_minimal_signed_body() -> TestResult {
    use figcraft_cloud_storage::webhook_events::{EventError, publish_event};
    let Some(pool) = pool().await? else { return Ok(()) };
    let receiver = Receiver::start(&[]).await?;
    let setup = setup(&pool, &receiver.url()).await?; // subscribed to everything
    let key = key()?;
    // A second endpoint that only wants exports, and a third that is disabled.
    let mut tx = begin_tenant_transaction(&pool, setup.tenant).await.map_err(|e| format!("{e:?}"))?;
    let (only_exports, _) = create_webhook_endpoint(&mut tx, &key, DEV, "host-a", &receiver.url(), &["export.completed".to_owned()])
        .await
        .map_err(|e| format!("{e:?}"))?;
    let (disabled, _) = create_webhook_endpoint(&mut tx, &key, DEV, "host-a", &receiver.url(), &[]).await.map_err(|e| format!("{e:?}"))?;
    update_webhook_endpoint(&mut tx, DEV, "host-a", disabled.id, None, None, Some(false)).await.map_err(|e| format!("{e:?}"))?;
    let data = [("document_id", "doc-uuid"), ("external_document_id", "host doc 7"), ("workspace_id", "ws-uuid")];
    assert_eq!(publish_event(&mut tx, "host-a", "document.created", &data).await.map_err(|e| format!("{e:?}"))?, 1);
    assert_eq!(
        publish_event(&mut tx, "host-a", "export.completed", &[("export_id", "e-1"), ("format", "png")]).await.map_err(|e| format!("{e:?}"))?,
        2
    );
    // Another host's endpoints and unknown hosts get nothing; bad events are refused up front.
    assert_eq!(publish_event(&mut tx, "no-such-host", "document.created", &data).await.map_err(|e| format!("{e:?}"))?, 0);
    assert_eq!(publish_event(&mut tx, "host-a", "document.created", &[("document_name", "Secret plans")]).await, Err(EventError::InvalidInput));
    assert_eq!(publish_event(&mut tx, "host-a", "document.exploded", &data).await, Err(EventError::InvalidInput));
    tx.commit().await?;

    // The everything endpoint got both events; the export endpoint only the export; the disabled
    // one none.
    let mut tx = begin_tenant_transaction(&pool, setup.tenant).await.map_err(|e| format!("{e:?}"))?;
    let everything = list_deliveries(&mut tx, "host-a", setup.endpoint, None).await.map_err(|e| format!("{e:?}"))?;
    let exports = list_deliveries(&mut tx, "host-a", only_exports.id, None).await.map_err(|e| format!("{e:?}"))?;
    let none = list_deliveries(&mut tx, "host-a", disabled.id, None).await.map_err(|e| format!("{e:?}"))?;
    tx.rollback().await?;
    assert_eq!(everything.len(), 2);
    assert_eq!((exports.len(), exports[0].event_type.as_str()), (1, "export.completed"));
    assert!(none.is_empty());
    // One event id is shared by every endpoint that got it.
    let shared = everything.iter().find(|d| d.event_type == "export.completed").ok_or("export delivery")?;
    assert_eq!(shared.event_id, exports[0].event_id);

    // Deliver the document event: the receiver sees the minimal body, correctly signed.
    let created = everything.iter().find(|d| d.event_type == "document.created").ok_or("created delivery")?;
    assert_eq!(attempt(&pool, &setup, created.id).await?, Outcome::Delivered { status: 200 });
    let request = &receiver.requests()[0];
    let body: serde_json::Value = serde_json::from_slice(&request.body)?;
    assert_eq!(body["type"], "document.created");
    assert_eq!(body["id"].as_str(), Some(created.event_id.to_string().as_str()));
    assert_eq!(body["tenant_id"].as_str(), Some(setup.tenant.to_string().as_str()));
    let mut keys: Vec<&str> = body["data"].as_object().ok_or("data")?.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["document_id", "external_document_id", "workspace_id"]);
    assert!(!String::from_utf8_lossy(&request.body).contains("Secret plans"));
    let timestamp: i64 = request.header("x-figcraft-timestamp").ok_or("timestamp")?.parse()?;
    assert!(verify_signature(
        setup.secret.as_bytes(),
        request.header("x-figcraft-signature").ok_or("signature")?,
        timestamp,
        &request.body,
        now(),
        SIGNATURE_TOLERANCE_SECS
    ));
    Ok(())
}

#[tokio::test]
async fn rotation_signs_with_both_secrets_during_the_overlap() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let receiver = Receiver::start(&[]).await?;
    let setup = setup(&pool, &receiver.url()).await?;
    let mut tx = begin_tenant_transaction(&pool, setup.tenant).await.map_err(|e| format!("{e:?}"))?;
    let new_secret = rotate_webhook_secret(&mut tx, &key()?, "host-a", setup.endpoint).await.map_err(|e| format!("{e:?}"))?.ok_or("endpoint")?;
    tx.commit().await?;
    let delivery = enqueue(&pool, &setup, Uuid::new_v4(), DEFAULT_MAX_ATTEMPTS).await?;
    assert_eq!(attempt(&pool, &setup, delivery).await?, Outcome::Delivered { status: 200 });
    let request = &receiver.requests()[0];
    let timestamp: i64 = request.header("x-figcraft-timestamp").ok_or("timestamp")?.parse()?;
    let signature = request.header("x-figcraft-signature").ok_or("signature")?;
    assert_eq!(signature.matches("v1=").count(), 2);
    for secret in [setup.secret.as_bytes(), new_secret.expose().as_bytes()] {
        assert!(verify_signature(secret, signature, timestamp, &request.body, now(), SIGNATURE_TOLERANCE_SECS));
    }
    Ok(())
}

#[tokio::test]
async fn disabled_endpoints_and_hosts_are_not_claimed_until_enabled_again() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let _serial = serial().lock().await;
    let receiver = Receiver::start(&[]).await?;
    let setup = setup(&pool, &receiver.url()).await?;
    let delivery = enqueue(&pool, &setup, Uuid::new_v4(), DEFAULT_MAX_ATTEMPTS).await?;
    let mine = |claimed: &[(TenantId, Uuid)]| claimed.iter().filter(|(tenant, _)| *tenant == setup.tenant).count();

    let mut tx = begin_tenant_transaction(&pool, setup.tenant).await.map_err(|e| format!("{e:?}"))?;
    update_webhook_endpoint(&mut tx, DEV, "host-a", setup.endpoint, None, None, Some(false)).await.map_err(|e| format!("{e:?}"))?;
    tx.commit().await?;
    assert_eq!(mine(&claim_due(&pool, 100).await.map_err(|e| format!("{e:?}"))?), 0, "a disabled endpoint is not claimed");

    let mut tx = begin_tenant_transaction(&pool, setup.tenant).await.map_err(|e| format!("{e:?}"))?;
    update_webhook_endpoint(&mut tx, DEV, "host-a", setup.endpoint, None, None, Some(true)).await.map_err(|e| format!("{e:?}"))?;
    set_host_enabled(&mut tx, "host-a", false).await.map_err(|e| format!("{e:?}"))?;
    tx.commit().await?;
    assert_eq!(mine(&claim_due(&pool, 100).await.map_err(|e| format!("{e:?}"))?), 0, "a disabled host is not claimed");

    let mut tx = begin_tenant_transaction(&pool, setup.tenant).await.map_err(|e| format!("{e:?}"))?;
    set_host_enabled(&mut tx, "host-a", true).await.map_err(|e| format!("{e:?}"))?;
    tx.commit().await?;
    let claimed = claim_due(&pool, 100).await.map_err(|e| format!("{e:?}"))?;
    assert_eq!(mine(&claimed), 1);
    // Claiming took a lease and counted the attempt; a second claim does not hand it out again.
    assert_eq!(mine(&claim_due(&pool, 100).await.map_err(|e| format!("{e:?}"))?), 0);
    assert_eq!(record(&pool, &setup, delivery).await?.attempts, 1);
    assert_eq!(process_claimed(&pool, &key()?, DEV, setup.tenant, delivery).await.map_err(|e| format!("{e:?}"))?, Outcome::Delivered { status: 200 });
    Ok(())
}

#[tokio::test]
async fn concurrent_workers_never_claim_the_same_delivery_twice() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let _serial = serial().lock().await;
    let receiver = Receiver::start(&[]).await?;
    let setup = setup(&pool, &receiver.url()).await?;
    let mut deliveries = Vec::new();
    for _ in 0..12 {
        deliveries.push(enqueue(&pool, &setup, Uuid::new_v4(), DEFAULT_MAX_ATTEMPTS).await?);
    }
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move { claim_due(&pool, 100).await.map_err(|e| format!("{e:?}")) }));
    }
    let mut mine = Vec::new();
    for task in tasks {
        mine.extend(task.await?.map_err(|e| e.to_string())?.into_iter().filter(|(tenant, _)| *tenant == setup.tenant).map(|(_, id)| id));
    }
    mine.sort();
    let mut expected = deliveries.clone();
    expected.sort();
    assert_eq!(mine, expected, "each of the 12 deliveries is claimed exactly once across 4 workers");
    Ok(())
}

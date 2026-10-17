//! The HTTP server on port 8081: health and readiness probes for every role, the API transport
//! probe and `/api/v1` for API roles, and the WebSocket upgrade diagnostic for relay roles.
//!
//! Every route runs behind [`crate::api::context::middleware`] (request ID, `traceparent`,
//! structured request log, error envelope, in-flight cap, deadline, declared body size). The
//! probe bodies and status codes are unchanged from the handwritten server this replaces.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocketUpgrade, rejection::WebSocketUpgradeRejection};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Version, header};
use axum::middleware::from_fn_with_state;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64::Engine as _;

use crate::api::context::{self, ContextSettings};
use crate::api::{self, ApiConfig, AppState};
use crate::config::{CloudConfig, CloudRole};

const HEALTH_ADDR: ([u8; 4], u16) = ([0, 0, 0, 0], 8081);
/// Concurrent requests served at once; the rest get `503`.
const MAX_IN_FLIGHT: usize = 1024;
/// A request that takes longer than this gets `503`.
const REQUEST_DEADLINE: Duration = Duration::from_secs(30);
/// Database connections the API holds at most.
const API_POOL_CONNECTIONS: u32 = 16;

pub(super) fn probe_ready() -> io::Result<bool> {
    probe_ready_at(SocketAddr::from(([127, 0, 0, 1], 8081)))
}

fn probe_ready_at(address: SocketAddr) -> io::Result<bool> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(b"GET /readyz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
    let mut status = [0; 17];
    stream.read_exact(&mut status)?;
    Ok(&status == b"HTTP/1.1 200 OK\r\n")
}

const fn serves_api(role: CloudRole) -> bool {
    matches!(role, CloudRole::All | CloudRole::Api)
}

const fn serves_relay(role: CloudRole) -> bool {
    matches!(role, CloudRole::All | CloudRole::Relay)
}

/// Serve port 8081 until the listener fails. `ready` is the dependency check made at startup;
/// `/api/v1` answers `503` while it is false.
pub(super) fn serve(config: &CloudConfig, ready: bool) -> io::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async {
        let api_config = ApiConfig { deployment: config.deployment, trusted_proxies: config.trusted_proxies.clone(), ..ApiConfig::default() };
        let max_body = api_config.max_body_bytes;
        let api = if serves_api(config.role) && ready { api_state(config, api_config).map(api::api_router) } else { None };
        if serves_api(config.role) && ready && api.is_none() {
            eprintln!("figcraft-cloud: the API could not be configured; /api/v1 will answer 503");
        }
        let settings = ContextSettings::new(config.role, MAX_IN_FLIGHT, REQUEST_DEADLINE, u64::try_from(max_body).unwrap_or(u64::MAX));
        let app = app(config.role, ready, api, settings);
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(HEALTH_ADDR)).await?;
        eprintln!(
            "figcraft-cloud: HTTP server listening on {} for role '{}' (dependencies_ready={ready})",
            SocketAddr::from(HEALTH_ADDR),
            config.role.as_str()
        );
        axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await
    })
}

/// The API's shared state: a lazily connecting pool and the object store.
fn api_state(config: &CloudConfig, api_config: ApiConfig) -> Option<AppState> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(API_POOL_CONNECTIONS)
        .acquire_timeout(Duration::from_secs(5))
        .connect_lazy(config.database_url()?)
        .ok()?;
    let objects = figcraft_cloud_storage::objects::ObjectStore::new(&config.object_store_settings()?).ok()?;
    Some(AppState::new(pool, objects, api_config))
}

/// The whole router for `role`. `api` is the `/api/v1` router when the API is configured; API
/// roles without it answer `503` under `/api/v1` (the transport probe still answers).
pub(crate) fn app(role: CloudRole, ready: bool, api: Option<Router>, settings: ContextSettings) -> Router {
    let mut router = Router::new()
        .route("/healthz", get(move || async move { json(StatusCode::OK, format!("{{\"status\":\"alive\",\"role\":\"{}\"}}", role.as_str())) }))
        .route("/startupz", get(move || async move { json(StatusCode::OK, format!("{{\"status\":\"started\",\"role\":\"{}\"}}", role.as_str())) }))
        .route("/readyz", get(move || async move { readiness(role, ready) }));
    if serves_api(role) {
        router = router.route("/api/v1/healthz", get(|| async { json(StatusCode::OK, "{\"status\":\"transport_ready\"}".to_string()) }));
        router = router.merge(api.unwrap_or_else(api::unavailable_router));
    }
    if serves_relay(role) {
        router = router.route("/ws/v1/health", get(websocket_probe));
    }
    router
        .fallback(legacy_fallback)
        .method_not_allowed_fallback(|| async { method_not_allowed() })
        .layer(from_fn_with_state(settings, context::middleware))
}

fn readiness(role: CloudRole, ready: bool) -> Response {
    if ready {
        json(StatusCode::OK, format!("{{\"status\":\"transport_ready\",\"role\":\"{}\"}}", role.as_str()))
    } else {
        json(StatusCode::SERVICE_UNAVAILABLE, format!("{{\"status\":\"not_ready\",\"role\":\"{}\"}}", role.as_str()))
    }
}

fn json(status: StatusCode, body: String) -> Response {
    (status, [(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))], body).into_response()
}

fn method_not_allowed() -> Response {
    json(StatusCode::METHOD_NOT_ALLOWED, "{\"error\":\"method not allowed\"}".to_string())
}

/// Outside `/api/v1`: `405` for anything but `GET` (as the old server did), `404` otherwise.
async fn legacy_fallback(method: Method) -> Response {
    if method == Method::GET { json(StatusCode::NOT_FOUND, "{\"error\":\"not found\"}".to_string()) } else { method_not_allowed() }
}

/// Prove the upgrade path: validate the handshake, switch protocols and close normally at once.
/// Collaboration traffic is served by the relay on port 8082.
async fn websocket_probe(version: Version, headers: HeaderMap, upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>) -> Response {
    let key_is_valid = headers
        .get(header::SEC_WEBSOCKET_KEY)
        .and_then(|key| base64::engine::general_purpose::STANDARD.decode(key.as_bytes()).ok())
        .is_some_and(|key| key.len() == 16);
    let upgrade = match upgrade {
        Ok(upgrade) if version == Version::HTTP_11 && headers.contains_key(header::HOST) && key_is_valid => upgrade,
        _ => return json(StatusCode::UPGRADE_REQUIRED, "{\"error\":\"websocket upgrade required\"}".to_string()),
    };
    upgrade.on_upgrade(|mut socket| async move {
        let _ = socket.send(Message::Close(Some(CloseFrame { code: 1000, reason: Utf8Bytes::from_static("") }))).await;
    })
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::str::FromStr;
    use std::thread;
    use std::time::Duration;

    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use figcraft_cloud_domain::RequestId;
    use tower::ServiceExt as _;

    use super::{app, probe_ready_at};
    use crate::api::context::ContextSettings;
    use crate::config::CloudRole;

    fn router(role: CloudRole, ready: bool) -> Router {
        app(role, ready, None, ContextSettings::new(role, 16, Duration::from_secs(5), 1024))
    }

    async fn call(role: CloudRole, ready: bool, request: Request<Body>) -> (StatusCode, axum::http::HeaderMap, String) {
        let response = router(role, ready).oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = to_bytes(response.into_body(), 1 << 16).await.unwrap();
        (status, headers, String::from_utf8(body.to_vec()).unwrap())
    }

    async fn get(role: CloudRole, ready: bool, path: &str) -> (StatusCode, String) {
        let (status, _, body) = call(role, ready, Request::get(path).body(Body::empty()).unwrap()).await;
        (status, body)
    }

    #[tokio::test]
    async fn liveness_startup_and_readiness_are_distinct_for_each_role() {
        for role in [CloudRole::All, CloudRole::Api, CloudRole::Relay, CloudRole::Worker] {
            let name = role.as_str();
            assert_eq!(get(role, false, "/healthz").await, (StatusCode::OK, format!("{{\"status\":\"alive\",\"role\":\"{name}\"}}")));
            assert_eq!(get(role, false, "/startupz").await, (StatusCode::OK, format!("{{\"status\":\"started\",\"role\":\"{name}\"}}")));
            assert_eq!(
                get(role, false, "/readyz").await,
                (StatusCode::SERVICE_UNAVAILABLE, format!("{{\"status\":\"not_ready\",\"role\":\"{name}\"}}"))
            );
            assert_eq!(get(role, true, "/readyz").await, (StatusCode::OK, format!("{{\"status\":\"transport_ready\",\"role\":\"{name}\"}}")));
        }
    }

    #[tokio::test]
    async fn api_transport_probe_is_available_only_in_api_roles() {
        for role in [CloudRole::All, CloudRole::Api] {
            assert_eq!(get(role, false, "/api/v1/healthz").await, (StatusCode::OK, "{\"status\":\"transport_ready\"}".to_string()));
            // Without a configured API every other API route is unavailable, in the envelope.
            let (status, body) = get(role, false, "/api/v1/tenants").await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            assert!(body.contains("\"code\":\"temporarily_unavailable\""), "{body}");
        }
        for role in [CloudRole::Relay, CloudRole::Worker] {
            assert_eq!(get(role, true, "/api/v1/healthz").await, (StatusCode::NOT_FOUND, "{\"error\":\"not found\"}".to_string()));
        }
    }

    #[tokio::test]
    async fn websocket_probe_requires_an_upgrade_and_only_in_relay_roles() {
        let handshake = || {
            Request::get("/ws/v1/health")
                .header("host", "localhost")
                .header("upgrade", "websocket")
                .header("connection", "keep-alive, Upgrade")
                .header("sec-websocket-version", "13")
                .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
                .body(Body::empty())
                .unwrap()
        };
        assert_eq!(
            get(CloudRole::All, true, "/ws/v1/health").await,
            (StatusCode::UPGRADE_REQUIRED, "{\"error\":\"websocket upgrade required\"}".to_string())
        );
        assert_eq!(call(CloudRole::Api, true, handshake()).await.0, StatusCode::NOT_FOUND);
        // A bad key is refused before any upgrade.
        let mut bad = handshake();
        bad.headers_mut().insert("sec-websocket-key", "c2hvcnQ=".parse().unwrap());
        assert_eq!(call(CloudRole::Relay, true, bad).await.0, StatusCode::UPGRADE_REQUIRED);
    }

    #[tokio::test]
    async fn unknown_paths_and_methods_fail_safely() {
        assert_eq!(get(CloudRole::Api, false, "/metrics").await, (StatusCode::NOT_FOUND, "{\"error\":\"not found\"}".to_string()));
        let post = |path: &str| Request::post(path).body(Body::empty()).unwrap();
        let (status, _, body) = call(CloudRole::Api, false, post("/healthz")).await;
        assert_eq!((status, body.as_str()), (StatusCode::METHOD_NOT_ALLOWED, "{\"error\":\"method not allowed\"}"));
        assert_eq!(call(CloudRole::Worker, false, post("/metrics")).await.0, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn request_ids_are_generated_and_returned() {
        let (_, headers, _) = call(CloudRole::Api, false, Request::get("/healthz").body(Body::empty()).unwrap()).await;
        let id = headers.get("x-request-id").unwrap().to_str().unwrap();
        assert!(RequestId::from_str(id).is_ok());
        let (_, again, _) = call(CloudRole::Api, false, Request::get("/healthz").body(Body::empty()).unwrap()).await;
        assert_ne!(again.get("x-request-id").unwrap(), id);
    }

    #[tokio::test]
    async fn valid_w3c_trace_context_is_forwarded_and_invalid_context_is_dropped() {
        let valid = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let request = Request::get("/healthz").header("traceparent", valid).body(Body::empty()).unwrap();
        let (_, headers, _) = call(CloudRole::Api, false, request).await;
        assert_eq!(headers.get("traceparent").unwrap(), valid);
        let invalid = "00-00000000000000000000000000000000-00f067aa0ba902b7-01";
        let request = Request::get("/healthz").header("traceparent", invalid).body(Body::empty()).unwrap();
        let (_, headers, _) = call(CloudRole::Api, false, request).await;
        assert!(headers.get("traceparent").is_none());
    }

    #[tokio::test]
    async fn declared_bodies_over_the_limit_are_refused_on_every_route() {
        let request = Request::get("/healthz").header("content-length", "4096").body(Body::from(vec![b'x'; 4096])).unwrap();
        let (status, _, body) = call(CloudRole::Api, false, request).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert!(body.contains("\"code\":\"invalid_request\""), "{body}");
    }

    #[tokio::test]
    async fn oversized_headers_are_refused() {
        let request = Request::get("/healthz").header("x-padding", "p".repeat(crate::api::context::MAX_HEADER_BYTES)).body(Body::empty()).unwrap();
        let (status, headers, body) = call(CloudRole::Worker, false, request).await;
        assert_eq!(status, StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE);
        assert!(headers.contains_key("x-request-id"));
        assert!(body.contains("\"code\":\"invalid_request\""), "{body}");
    }

    /// Serve `role` on a loopback listener in a background runtime; returns its address.
    fn spawn_server(role: CloudRole, ready: bool) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let app = router(role, ready);
                axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
            });
        });
        address
    }

    fn exchange(address: SocketAddr, request: &[u8]) -> Vec<u8> {
        let mut client = TcpStream::connect(address).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        client.write_all(request).unwrap();
        let mut response = Vec::new();
        let _ = client.read_to_end(&mut response);
        response
    }

    #[test]
    fn health_endpoint_serves_http_over_a_local_socket() {
        let address = spawn_server(CloudRole::Relay, false);
        let response = exchange(address, b"GET /readyz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 503 Service Unavailable"), "{response}");
        assert!(response.contains("\"role\":\"relay\""));
        assert!(response.to_ascii_lowercase().contains("x-request-id: "));
        // Malformed requests are refused by the HTTP parser.
        let garbage = String::from_utf8(exchange(address, b"not http\r\n\r\n")).unwrap();
        assert!(garbage.is_empty() || garbage.starts_with("HTTP/1.1 400"), "{garbage}");
        // The readiness probe used by `--check-ready` reads the real server.
        assert!(!probe_ready_at(address).unwrap());
        assert!(probe_ready_at(spawn_server(CloudRole::Api, true)).unwrap());
    }

    #[test]
    fn websocket_probe_returns_the_rfc_accept_value_and_a_normal_close_frame() {
        let address = spawn_server(CloudRole::All, true);
        let response = exchange(
            address,
            b"GET /ws/v1/health HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
        );
        let text = String::from_utf8_lossy(&response).to_ascii_lowercase();
        assert!(response.starts_with(b"HTTP/1.1 101 Switching Protocols\r\n"), "{text}");
        assert!(text.contains("sec-websocket-accept: s3pplmbitxaq9kygzzhzrbk+xoo=\r\n"), "{text}");
        assert!(text.contains("x-request-id: "));
        assert!(response.ends_with(&[0x88, 0x02, 0x03, 0xE8]), "{response:?}");
    }

    #[test]
    fn readiness_probe_accepts_only_http_200() {
        for (reply, expected) in
            [("HTTP/1.1 200 OK\r\n\r\n", true), ("HTTP/1.1 503 Service Unavailable\r\n\r\n", false), ("HTTP/1.1 200 NO\r\n\r\n", false)]
        {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind test socket");
            let address = listener.local_addr().expect("test socket address");
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept probe");
                let mut request = [0; 128];
                let count = stream.read(&mut request).expect("read probe");
                assert!(std::str::from_utf8(&request[..count]).expect("UTF-8 probe").starts_with("GET /readyz HTTP/1.1"));
                stream.write_all(reply.as_bytes()).expect("write probe response");
            });
            assert_eq!(probe_ready_at(address).expect("probe response"), expected);
            server.join().expect("probe server thread");
        }
    }
}

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use figcraft_cloud_domain::RequestId;
use sha1::{Digest, Sha1};

use crate::config::CloudRole;

const MAX_REQUEST_BYTES: usize = 8 * 1024;
const HEALTH_ADDR: &str = "0.0.0.0:8081";
const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

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

pub(super) fn serve(role: CloudRole, ready: bool) -> io::Result<()> {
    let listener = TcpListener::bind(HEALTH_ADDR)?;
    eprintln!("figcraft-cloud: health endpoints listening on {HEALTH_ADDR} for role '{}' (dependencies_ready={ready})", role.as_str());
    for stream in listener.incoming() {
        let mut stream = stream?;
        handle_connection(&mut stream, role, ready)?;
    }
    Ok(())
}

fn handle_connection(stream: &mut TcpStream, role: CloudRole, ready: bool) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let request = read_request(stream);
    let context = RequestContext::from_request(request.as_ref().ok().map(String::as_str));
    let result = match request {
        Ok(request) => route(&request, role, ready),
        Err(RequestError::TooLarge) => RouteResult::Http(413, "Payload Too Large", "{\"error\":\"request too large\"}".to_string()),
        Err(RequestError::Invalid) => RouteResult::Http(400, "Bad Request", "{\"error\":\"invalid request\"}".to_string()),
    };
    let status = match result {
        RouteResult::Http(status, reason, body) => {
            let response = response(status, reason, &body, &context);
            stream.write_all(response.as_bytes())?;
            status
        }
        RouteResult::WebSocket(accept) => {
            let response = websocket_response(&accept, &context);
            stream.write_all(response.as_bytes())?;
            // The probe proves the upgrade path and immediately closes with a normal WebSocket
            // close frame. Collaboration messages are implemented by a later relay milestone.
            stream.write_all(&[0x88, 0x02, 0x03, 0xE8])?;
            101
        }
    };
    stream.flush()?;
    write_request_log(role, status, &context);
    Ok(())
}

enum RouteResult {
    Http(u16, &'static str, String),
    WebSocket(String),
}

#[derive(Clone, Copy)]
enum RequestError {
    TooLarge,
    Invalid,
}

fn read_request(stream: &mut TcpStream) -> Result<String, RequestError> {
    let mut request = Vec::with_capacity(1024);
    let mut chunk = [0; 1024];
    loop {
        let count = stream.read(&mut chunk).map_err(|_| RequestError::Invalid)?;
        if count == 0 {
            return Err(RequestError::Invalid);
        }
        if request.len().saturating_add(count) > MAX_REQUEST_BYTES {
            return Err(RequestError::TooLarge);
        }
        request.extend_from_slice(&chunk[..count]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            return String::from_utf8(request).map_err(|_| RequestError::Invalid);
        }
    }
}

struct RequestContext {
    request_id: RequestId,
    traceparent: Option<TraceParent>,
}

impl RequestContext {
    fn from_request(request: Option<&str>) -> Self {
        let traceparent = request.and_then(parse_traceparent);
        Self { request_id: RequestId::random(), traceparent }
    }
}

struct TraceParent {
    value: String,
    trace_id: String,
    parent_id: String,
}

fn parse_traceparent(request: &str) -> Option<TraceParent> {
    let value = request.lines().skip(1).take_while(|line| !line.is_empty()).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("traceparent").then(|| value.trim())
    })?;
    let mut parts = value.split('-');
    let version = parts.next()?;
    let trace_id = parts.next()?;
    let parent_id = parts.next()?;
    let flags = parts.next()?;
    if parts.next().is_some()
        || version != "00"
        || trace_id.len() != 32
        || parent_id.len() != 16
        || flags.len() != 2
        || !trace_id.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !parent_id.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !flags.bytes().all(|byte| byte.is_ascii_hexdigit())
        || trace_id.bytes().all(|byte| byte == b'0')
        || parent_id.bytes().all(|byte| byte == b'0')
    {
        return None;
    }
    Some(TraceParent { value: value.to_string(), trace_id: trace_id.to_string(), parent_id: parent_id.to_string() })
}

fn route(request: &str, role: CloudRole, ready: bool) -> RouteResult {
    let Some(line) = request.lines().next() else {
        return RouteResult::Http(400, "Bad Request", r#"{"error":"invalid request"}"#.to_string());
    };
    let mut parts = line.split_ascii_whitespace();
    let (Some(method), Some(path), Some(version)) = (parts.next(), parts.next(), parts.next()) else {
        return RouteResult::Http(400, "Bad Request", "{\"error\":\"invalid request\"}".to_string());
    };
    if parts.next().is_some() || (version != "HTTP/1.0" && version != "HTTP/1.1") {
        return RouteResult::Http(400, "Bad Request", "{\"error\":\"invalid request\"}".to_string());
    }
    if method != "GET" {
        return RouteResult::Http(405, "Method Not Allowed", "{\"error\":\"method not allowed\"}".to_string());
    }

    if path == "/api/v1/healthz" && matches!(role, CloudRole::All | CloudRole::Api) {
        return RouteResult::Http(200, "OK", "{\"status\":\"transport_ready\"}".to_string());
    }
    if path == "/ws/v1/health" && matches!(role, CloudRole::All | CloudRole::Relay) {
        return match websocket_accept(request) {
            Some(accept) => RouteResult::WebSocket(accept),
            None => RouteResult::Http(426, "Upgrade Required", "{\"error\":\"websocket upgrade required\"}".to_string()),
        };
    }

    match path {
        "/healthz" => RouteResult::Http(200, "OK", format!("{{\"status\":\"alive\",\"role\":\"{}\"}}", role.as_str())),
        "/startupz" => RouteResult::Http(200, "OK", format!("{{\"status\":\"started\",\"role\":\"{}\"}}", role.as_str())),
        "/readyz" if ready => RouteResult::Http(200, "OK", format!("{{\"status\":\"transport_ready\",\"role\":\"{}\"}}", role.as_str())),
        "/readyz" => RouteResult::Http(503, "Service Unavailable", format!("{{\"status\":\"not_ready\",\"role\":\"{}\"}}", role.as_str())),
        _ => RouteResult::Http(404, "Not Found", "{\"error\":\"not found\"}".to_string()),
    }
}

fn websocket_accept(request: &str) -> Option<String> {
    if request.lines().next()?.split_ascii_whitespace().nth(2)? != "HTTP/1.1" {
        return None;
    }
    let header = |name: &str| {
        request.lines().skip(1).take_while(|line| !line.is_empty()).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then(|| value.trim())
        })
    };
    header("host")?;
    let upgrade = header("upgrade")?;
    let connection = header("connection")?;
    let version = header("sec-websocket-version")?;
    let key = header("sec-websocket-key")?;
    if !upgrade.eq_ignore_ascii_case("websocket")
        || !connection.split(',').any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        || version != "13"
        || base64::engine::general_purpose::STANDARD.decode(key).ok()?.len() != 16
    {
        return None;
    }
    let mut digest = Sha1::new();
    digest.update(key.as_bytes());
    digest.update(WEBSOCKET_GUID.as_bytes());
    Some(base64::engine::general_purpose::STANDARD.encode(digest.finalize()))
}

fn response(status: u16, reason: &str, body: &str, context: &RequestContext) -> String {
    let trace_header = context.traceparent.as_ref().map(|trace| format!("traceparent: {}\r\n", trace.value)).unwrap_or_default();
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nX-Request-Id: {}\r\n{trace_header}Connection: close\r\n\r\n{body}",
        body.len(),
        context.request_id
    )
}

fn websocket_response(accept: &str, context: &RequestContext) -> String {
    format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\nX-Request-Id: {}\r\n\r\n",
        context.request_id
    )
}

fn write_request_log(role: CloudRole, status: u16, context: &RequestContext) {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| duration.as_millis());
    let mut record = serde_json::json!({
        "timestamp_unix_ms": timestamp,
        "level": "info",
        "event": "http_request",
        "request_id": context.request_id.to_string(),
        "role": role.as_str(),
        "status": status,
    });
    if let Some(traceparent) = &context.traceparent {
        record["trace_id"] = serde_json::Value::String(traceparent.trace_id.clone());
        record["parent_span_id"] = serde_json::Value::String(traceparent.parent_id.clone());
    }
    if let Ok(line) = serde_json::to_string(&record) {
        let _ = writeln!(io::stdout().lock(), "{line}");
    }
}

#[cfg(test)]
fn response_for(request: &str, role: CloudRole, ready: bool) -> String {
    let context = RequestContext::from_request(Some(request));
    match route(request, role, ready) {
        RouteResult::Http(status, reason, body) => response(status, reason, &body, &context),
        RouteResult::WebSocket(accept) => websocket_response(&accept, &context),
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::str::FromStr;
    use std::thread;

    use figcraft_cloud_domain::RequestId;

    use super::{handle_connection, parse_traceparent, probe_ready_at, response_for};
    use crate::config::CloudRole;

    #[test]
    fn liveness_startup_and_readiness_are_distinct_for_each_role() {
        for role in [CloudRole::All, CloudRole::Api, CloudRole::Relay, CloudRole::Worker] {
            let health = response_for("GET /healthz HTTP/1.1\r\n\r\n", role, false);
            let startup = response_for("GET /startupz HTTP/1.1\r\n\r\n", role, false);
            let ready = response_for("GET /readyz HTTP/1.1\r\n\r\n", role, false);
            assert!(health.starts_with("HTTP/1.1 200 OK"));
            assert!(health.contains(role.as_str()));
            assert!(startup.starts_with("HTTP/1.1 200 OK"));
            assert!(startup.contains("started"));
            assert!(ready.starts_with("HTTP/1.1 503 Service Unavailable"));
            assert!(ready.contains("not_ready"));
            assert!(response_for("GET /readyz HTTP/1.1\r\n\r\n", role, true).starts_with("HTTP/1.1 200 OK"));
        }
    }

    #[test]
    fn api_transport_probe_is_available_only_in_api_roles() {
        let request = "GET /api/v1/healthz HTTP/1.1\r\n\r\n";
        assert!(response_for(request, CloudRole::All, true).starts_with("HTTP/1.1 200 OK"));
        assert!(response_for(request, CloudRole::Api, true).contains("transport_ready"));
        assert!(response_for(request, CloudRole::Relay, true).starts_with("HTTP/1.1 404"));
        assert!(response_for(request, CloudRole::Worker, true).starts_with("HTTP/1.1 404"));
    }

    #[test]
    fn websocket_probe_validates_the_upgrade_and_returns_the_rfc_accept_value() {
        let request = concat!(
            "GET /ws/v1/health HTTP/1.1\r\n",
            "Host: localhost\r\n",
            "Upgrade: websocket\r\n",
            "Connection: keep-alive, Upgrade\r\n",
            "Sec-WebSocket-Version: 13\r\n",
            "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
        );
        let response = response_for(request, CloudRole::All, true);
        assert!(response.starts_with("HTTP/1.1 101 Switching Protocols"));
        assert!(response.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"));
        assert!(response_for("GET /ws/v1/health HTTP/1.1\r\n\r\n", CloudRole::All, true).starts_with("HTTP/1.1 426"));
        assert!(response_for(request, CloudRole::Api, true).starts_with("HTTP/1.1 404"));
    }

    #[test]
    fn unknown_paths_methods_and_malformed_requests_fail_safely() {
        assert!(response_for("GET /metrics HTTP/1.1\r\n\r\n", CloudRole::Api, false).starts_with("HTTP/1.1 404"));
        assert!(response_for("POST /healthz HTTP/1.1\r\n\r\n", CloudRole::Api, false).starts_with("HTTP/1.1 405"));
        assert!(response_for("not http", CloudRole::Api, false).starts_with("HTTP/1.1 400"));
    }

    #[test]
    fn request_ids_are_generated_and_returned() {
        let response = response_for("GET /healthz HTTP/1.1\r\n\r\n", CloudRole::Api, false);
        let id = response.lines().find_map(|line| line.strip_prefix("X-Request-Id: ")).expect("request ID header");
        assert!(RequestId::from_str(id).is_ok());
    }

    #[test]
    fn valid_w3c_trace_context_is_forwarded_and_invalid_context_is_dropped() {
        let valid = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let request = format!("GET /healthz HTTP/1.1\r\ntraceparent: {valid}\r\n\r\n");
        assert!(response_for(&request, CloudRole::Api, false).contains(&format!("traceparent: {valid}\r\n")));
        assert!(parse_traceparent("GET /healthz HTTP/1.1\r\ntraceparent: 00-00000000000000000000000000000000-00f067aa0ba902b7-01\r\n\r\n").is_none());
    }

    #[test]
    fn health_endpoint_serves_http_over_a_local_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test socket");
        let address = listener.local_addr().expect("test socket address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept test request");
            handle_connection(&mut stream, CloudRole::Relay, false).expect("serve test request");
        });

        let mut client = TcpStream::connect(address).expect("connect test socket");
        client.write_all(b"GET /readyz HTTP/1.1\r\nHost: localhost\r\n\r\n").expect("write test request");
        let mut response = String::new();
        client.read_to_string(&mut response).expect("read test response");
        server.join().expect("health server thread");
        assert!(response.starts_with("HTTP/1.1 503 Service Unavailable"));
        assert!(response.contains("\"role\":\"relay\""));
        assert!(response.contains("X-Request-Id: "));
    }

    #[test]
    fn websocket_probe_writes_a_normal_close_frame_after_upgrade() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test socket");
        let address = listener.local_addr().expect("test socket address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept test request");
            handle_connection(&mut stream, CloudRole::All, true).expect("serve websocket probe");
        });
        let mut client = TcpStream::connect(address).expect("connect test socket");
        client
            .write_all(
                b"GET /ws/v1/health HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
            )
            .expect("write websocket handshake");
        let mut response = Vec::new();
        client.read_to_end(&mut response).expect("read websocket close");
        server.join().expect("websocket handler thread");
        assert!(response.starts_with(b"HTTP/1.1 101 Switching Protocols\r\n"));
        assert!(response.ends_with(&[0x88, 0x02, 0x03, 0xE8]));
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

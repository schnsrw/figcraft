use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use crate::config::CloudRole;

const MAX_REQUEST_BYTES: usize = 8 * 1024;
const HEALTH_ADDR: &str = "0.0.0.0:8081";

pub(super) fn serve(role: CloudRole) -> io::Result<()> {
    let listener = TcpListener::bind(HEALTH_ADDR)?;
    eprintln!("figcraft-cloud: health endpoints listening on {HEALTH_ADDR} for role '{}'", role.as_str());
    for stream in listener.incoming() {
        let mut stream = stream?;
        handle_connection(&mut stream, role)?;
    }
    Ok(())
}

fn handle_connection(stream: &mut TcpStream, role: CloudRole) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let response = match read_request(stream) {
        Ok(request) => response_for(&request, role),
        Err(RequestError::TooLarge) => response(413, "Payload Too Large", "{\"error\":\"request too large\"}"),
        Err(RequestError::Invalid) => response(400, "Bad Request", "{\"error\":\"invalid request\"}"),
    };
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

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

fn response_for(request: &str, role: CloudRole) -> String {
    let Some(line) = request.lines().next() else {
        return response(400, "Bad Request", "{\"error\":\"invalid request\"}");
    };
    let mut parts = line.split_ascii_whitespace();
    let (Some(method), Some(path), Some(version)) = (parts.next(), parts.next(), parts.next()) else {
        return response(400, "Bad Request", "{\"error\":\"invalid request\"}");
    };
    if parts.next().is_some() || (version != "HTTP/1.0" && version != "HTTP/1.1") {
        return response(400, "Bad Request", "{\"error\":\"invalid request\"}");
    }
    if method != "GET" {
        return response(405, "Method Not Allowed", "{\"error\":\"method not allowed\"}");
    }

    match path {
        "/healthz" => response(200, "OK", &format!("{{\"status\":\"alive\",\"role\":\"{}\"}}", role.as_str())),
        "/startupz" => response(200, "OK", &format!("{{\"status\":\"started\",\"role\":\"{}\"}}", role.as_str())),
        "/readyz" => response(503, "Service Unavailable", &format!("{{\"status\":\"not_ready\",\"role\":\"{}\"}}", role.as_str())),
        _ => response(404, "Not Found", "{\"error\":\"not found\"}"),
    }
}

fn response(status: u16, reason: &str, body: &str) -> String {
    format!("HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    use super::{handle_connection, response_for};
    use crate::config::CloudRole;

    #[test]
    fn liveness_startup_and_readiness_are_distinct_for_each_role() {
        for role in [CloudRole::All, CloudRole::Api, CloudRole::Relay, CloudRole::Worker] {
            let health = response_for("GET /healthz HTTP/1.1\r\n\r\n", role);
            let startup = response_for("GET /startupz HTTP/1.1\r\n\r\n", role);
            let ready = response_for("GET /readyz HTTP/1.1\r\n\r\n", role);
            assert!(health.starts_with("HTTP/1.1 200 OK"));
            assert!(health.contains(role.as_str()));
            assert!(startup.starts_with("HTTP/1.1 200 OK"));
            assert!(startup.contains("started"));
            assert!(ready.starts_with("HTTP/1.1 503 Service Unavailable"));
            assert!(ready.contains("not_ready"));
        }
    }

    #[test]
    fn unknown_paths_methods_and_malformed_requests_fail_safely() {
        assert!(response_for("GET /metrics HTTP/1.1\r\n\r\n", CloudRole::Api).starts_with("HTTP/1.1 404"));
        assert!(response_for("POST /healthz HTTP/1.1\r\n\r\n", CloudRole::Api).starts_with("HTTP/1.1 405"));
        assert!(response_for("not http", CloudRole::Api).starts_with("HTTP/1.1 400"));
    }

    #[test]
    fn health_endpoint_serves_http_over_a_local_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test socket");
        let address = listener.local_addr().expect("test socket address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept test request");
            handle_connection(&mut stream, CloudRole::Relay).expect("serve test request");
        });

        let mut client = TcpStream::connect(address).expect("connect test socket");
        client.write_all(b"GET /readyz HTTP/1.1\r\nHost: localhost\r\n\r\n").expect("write test request");
        let mut response = String::new();
        client.read_to_string(&mut response).expect("read test response");
        server.join().expect("health server thread");
        assert!(response.starts_with("HTTP/1.1 503 Service Unavailable"));
        assert!(response.contains("\"role\":\"relay\""));
    }
}

//! A small blocking HTTP/1.1 client for the asset routes (native; the browser uses `fetch`):
//! one request per connection (`Connection: close`), `http://` or `https://` (through the same
//! TLS as `wss://`), no redirects, no proxies. It moves image files, so bodies are written in
//! chunks and every size read from the peer is bounded.

use std::io::{Read, Write};
use std::time::Duration;

use crate::invite::Endpoint;
use crate::net::{self, NetLimits};

/// Longest status line plus headers accepted.
const MAX_HEAD: usize = 32 * 1024;
/// How much of a request body is handed to the socket at once.
const WRITE_CHUNK: usize = 256 * 1024;

/// What the server answered.
#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

/// One request.
pub struct Request<'a> {
    /// `GET`, `PUT`…
    pub method: &'a str,
    /// An absolute `http://` or `https://` URL.
    pub url: &'a str,
    /// Headers besides `Host`, `Connection` and `Content-Length`, which are set here.
    pub headers: &'a [(&'a str, &'a str)],
    pub body: &'a [u8],
}

/// Where a URL points: the endpoint (host with port, TLS or not) and the path with its query.
fn target(url: &str) -> Result<(Endpoint, String), String> {
    let (secure, rest) = match (url.strip_prefix("https://"), url.strip_prefix("http://")) {
        (Some(rest), _) => (true, rest),
        (None, Some(rest)) => (false, rest),
        _ => return Err("not an http(s) address".into()),
    };
    let (authority, path) = rest.split_once('/').map_or((rest, "/".to_string()), |(a, p)| (a, format!("/{p}")));
    if authority.is_empty() || authority.contains(['@', ' ', '\r', '\n']) || path.contains([' ', '\r', '\n']) {
        return Err("not an http(s) address".into());
    }
    let has_port = authority.rsplit_once(':').is_some_and(|(_, port)| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()));
    let host = if has_port { authority.to_string() } else { format!("{authority}:{}", if secure { 443 } else { 80 }) };
    Ok((Endpoint { host, path: "/".into(), secure }, path))
}

fn header_ok(name: &str, value: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_graphic() && b != b':') && !value.contains(['\r', '\n'])
}

/// Send `request` and read the answer, which may be up to `max_response` bytes. `connect` bounds
/// the connection, `answer` the wait for each part of the reply (a server may take a while to
/// check and store a large upload before it answers).
pub fn send(request: &Request, max_response: usize, connect: Duration, answer: Duration, limits: &NetLimits) -> Result<Response, String> {
    let (endpoint, path) = target(request.url)?;
    if request.headers.iter().any(|(n, v)| !header_ok(n, v)) || !request.method.bytes().all(|b| b.is_ascii_uppercase()) {
        return Err("a header isn't valid".into());
    }
    let (mut stream, _, sock) = net::open(&endpoint, connect, limits)?;
    let result = exchange(&mut stream, &endpoint, &path, request, max_response, answer);
    net::shutdown(&sock);
    result
}

fn exchange(stream: &mut net::Duplex, endpoint: &Endpoint, path: &str, request: &Request, max_response: usize, answer: Duration) -> Result<Response, String> {
    let io = |e: std::io::Error| format!("the connection to {} failed: {e}", endpoint.host);
    stream.set_read_timeout(Some(answer)).map_err(io)?;
    let mut head = format!("{} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: figcraft\r\n", request.method, endpoint.host);
    for (name, value) in request.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if !request.body.is_empty() || request.method == "PUT" || request.method == "POST" {
        head.push_str(&format!("Content-Length: {}\r\n", request.body.len()));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).map_err(io)?;
    for chunk in request.body.chunks(WRITE_CHUNK) {
        stream.write_all(chunk).map_err(io)?;
    }
    stream.flush().map_err(io)?;
    read_response(stream, request.method == "HEAD", max_response).map_err(|e| format!("{} answered badly: {e}", endpoint.host))
}

/// Read a response from `stream`.
fn read_response(stream: &mut impl Read, head_only: bool, max_body: usize) -> Result<Response, String> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 16 * 1024];
    let end = loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        if buf.len() > MAX_HEAD {
            return Err("the headers are too long".into());
        }
        let n = stream.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("the connection closed before the headers ended".into());
        }
        buf.extend_from_slice(chunk.get(..n).unwrap_or_default());
    };
    let head = std::str::from_utf8(buf.get(..end).unwrap_or_default()).map_err(|_| "the headers are not text")?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.split(' ');
    let (Some(version), Some(code)) = (parts.next(), parts.next()) else { return Err("no status line".into()) };
    let status: u16 = code.parse().map_err(|_| "the status isn't a number")?;
    if !version.starts_with("HTTP/1.") {
        return Err("not an HTTP/1 answer".into());
    }
    let (mut length, mut chunked) = (None, false);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            length = Some(value.parse::<usize>().map_err(|_| "content-length isn't a number")?);
        } else if name.eq_ignore_ascii_case("transfer-encoding") && value.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        }
    }
    let mut rest: Vec<u8> = buf.get(end + 4..).unwrap_or_default().to_vec();
    if head_only || status == 204 || status == 304 || (100..200).contains(&status) {
        return Ok(Response { status, body: Vec::new() });
    }
    let body = if chunked {
        read_chunked(stream, &mut rest, max_body)?
    } else if let Some(n) = length {
        if n > max_body {
            return Err(format!("the body is {n} bytes, more than the {max_body} accepted"));
        }
        while rest.len() < n {
            let want = (n - rest.len()).min(chunk.len());
            let got = stream.read(chunk.get_mut(..want).unwrap_or_default()).map_err(|e| e.to_string())?;
            if got == 0 {
                return Err("the connection closed before the body ended".into());
            }
            rest.extend_from_slice(chunk.get(..got).unwrap_or_default());
        }
        rest.truncate(n);
        rest
    } else {
        loop {
            if rest.len() > max_body {
                return Err(format!("the body is more than the {max_body} bytes accepted"));
            }
            let got = stream.read(&mut chunk).map_err(|e| e.to_string())?;
            if got == 0 {
                break;
            }
            if rest.len() + got > max_body {
                return Err(format!("the body is more than the {max_body} bytes accepted"));
            }
            rest.extend_from_slice(chunk.get(..got).unwrap_or_default());
        }
        rest
    };
    Ok(Response { status, body })
}

/// A `Transfer-Encoding: chunked` body; `pending` holds what was read past the headers.
fn read_chunked(stream: &mut impl Read, pending: &mut Vec<u8>, max_body: usize) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    let mut scratch = [0u8; 16 * 1024];
    // More bytes for `pending` (false: the stream ended).
    let mut fill = |pending: &mut Vec<u8>, stream: &mut dyn Read| -> Result<bool, String> {
        let n = stream.read(&mut scratch).map_err(|e| e.to_string())?;
        pending.extend_from_slice(scratch.get(..n).unwrap_or_default());
        Ok(n > 0)
    };
    loop {
        // The size line.
        let line_end = loop {
            if let Some(at) = pending.windows(2).position(|w| w == b"\r\n") {
                break at;
            }
            if pending.len() > 1024 || !fill(pending, stream)? {
                return Err("a chunk header is missing".into());
            }
        };
        let line = std::str::from_utf8(pending.get(..line_end).unwrap_or_default()).map_err(|_| "a chunk size isn't text")?;
        let size = usize::from_str_radix(line.split(';').next().unwrap_or_default().trim(), 16).map_err(|_| "a chunk size isn't a number")?;
        pending.drain(..line_end + 2);
        if size == 0 {
            return Ok(body);
        }
        if body.len().saturating_add(size) > max_body {
            return Err(format!("the body is more than the {max_body} bytes accepted"));
        }
        while pending.len() < size + 2 {
            if !fill(pending, stream)? {
                return Err("the connection closed inside a chunk".into());
            }
        }
        body.extend_from_slice(pending.get(..size).unwrap_or_default());
        pending.drain(..size + 2);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::net::TcpListener;

    use super::*;

    fn read(bytes: &[u8], max: usize) -> Result<Response, String> {
        read_response(&mut Cursor::new(bytes.to_vec()), false, max)
    }

    #[test]
    fn urls_become_endpoints() {
        let (e, p) = target("http://127.0.0.1:8080/api/v1/x?y=1").unwrap();
        assert_eq!((e.host.as_str(), e.secure, p.as_str()), ("127.0.0.1:8080", false, "/api/v1/x?y=1"));
        let (e, p) = target("https://cloud.example/a").unwrap();
        assert_eq!((e.host.as_str(), e.secure, p.as_str()), ("cloud.example:443", true, "/a"));
        let (e, p) = target("http://[::1]:9/").unwrap();
        assert_eq!((e.host.as_str(), p.as_str()), ("[::1]:9", "/"));
        assert_eq!(target("http://h").unwrap().1, "/");
        for bad in ["ftp://h/", "h/a", "http:///a", "http://u@h/a", "http://h/a b", "http://h/a\r\nX: y"] {
            assert!(target(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn answers_with_a_length_are_read_exactly() {
        let r = read(b"HTTP/1.1 201 Created\r\nContent-Length: 5\r\nX: y\r\n\r\nhello-and-more", 100).unwrap();
        assert_eq!((r.status, r.body.as_slice()), (201, b"hello".as_slice()));
        assert!(read(b"HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\nshort", 100).is_err(), "cut off");
        assert!(read(b"HTTP/1.1 200 OK\r\nContent-Length: 101\r\n\r\n", 100).is_err(), "over the bound, refused before reading");
        assert!(read(b"HTTP/1.1 200 OK\r\nContent-Length: nope\r\n\r\n", 100).is_err());
    }

    #[test]
    fn chunked_and_unbounded_answers_are_read_within_the_bound() {
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n4;ext=1\r\ndefg\r\n0\r\n\r\n";
        assert_eq!(read(chunked, 100).unwrap().body, b"abcdefg");
        assert!(read(chunked, 5).is_err());
        assert!(read(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\nabc", 100).is_err());
        assert!(read(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nab", 100).is_err());
        assert_eq!(read(b"HTTP/1.1 200 OK\r\n\r\nuntil close", 100).unwrap().body, b"until close");
        assert!(read(b"HTTP/1.1 200 OK\r\n\r\nuntil close", 3).is_err());
        assert!(read(b"HTTP/1.1 304 Not Modified\r\nContent-Length: 9\r\n\r\n", 100).unwrap().body.is_empty());
    }

    #[test]
    fn junk_is_an_error_not_a_panic() {
        for junk in [&b""[..], b"\r\n\r\n", b"HTTP/1.1\r\n\r\n", b"SPDY/3 200 OK\r\n\r\n", b"HTTP/1.1 abc OK\r\n\r\n", &[0xff; 64]] {
            assert!(read(junk, 100).is_err(), "{junk:?}");
        }
        let mut long = b"HTTP/1.1 200 OK\r\nX: ".to_vec();
        long.extend(std::iter::repeat_n(b'a', MAX_HEAD + 100));
        assert!(read(&long, 100).is_err());
    }

    /// A server that reads one request and answers `reply`; returns the request it saw.
    fn serve_once(reply: &'static [u8]) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut seen = Vec::new();
            let mut chunk = [0u8; 8192];
            // Read the head, then as many body bytes as it says.
            let (head_end, need) = loop {
                let n = sock.read(&mut chunk).unwrap();
                seen.extend_from_slice(&chunk[..n]);
                if let Some(at) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&seen[..at]).to_ascii_lowercase();
                    let len = head.lines().find_map(|l| l.strip_prefix("content-length: ")).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
                    break (at + 4, len);
                }
            };
            while seen.len() < head_end + need {
                let n = sock.read(&mut chunk).unwrap();
                seen.extend_from_slice(&chunk[..n]);
            }
            sock.write_all(reply).unwrap();
            seen
        });
        (format!("http://{addr}/api/v1/documents/d/images/h"), handle)
    }

    #[test]
    fn a_put_and_a_get_go_over_a_real_socket() {
        let limits = NetLimits::default();
        let (url, server) = serve_once(b"HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
        let body = vec![7u8; 3 * 1024 * 1024];
        let headers = [("X-Figcraft-Room-Token", "secret"), ("Content-Type", "application/octet-stream")];
        let reply = send(&Request { method: "PUT", url: &url, headers: &headers, body: &body }, 1 << 20, Duration::from_secs(5), Duration::from_secs(10), &limits).unwrap();
        assert_eq!((reply.status, reply.body.as_slice()), (201, b"{}".as_slice()));
        let seen = server.join().unwrap();
        let text = String::from_utf8_lossy(&seen[..400]).to_string();
        assert!(text.starts_with("PUT /api/v1/documents/d/images/h HTTP/1.1\r\n"), "{text}");
        assert!(text.contains("X-Figcraft-Room-Token: secret\r\n") && text.contains(&format!("Content-Length: {}\r\n", body.len())), "{text}");
        assert!(seen.ends_with(&body), "the whole body arrived");

        let (url, server) = serve_once(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nPNG!");
        let reply = send(&Request { method: "GET", url: &url, headers: &headers[..1], body: &[] }, 1 << 20, Duration::from_secs(5), Duration::from_secs(10), &limits).unwrap();
        assert_eq!((reply.status, reply.body.as_slice()), (200, b"PNG!".as_slice()));
        let seen = server.join().unwrap();
        assert!(!String::from_utf8_lossy(&seen).contains("Content-Length"), "a GET declares no body");
    }

    #[test]
    fn a_refused_connection_and_bad_headers_are_errors() {
        let limits = NetLimits::default();
        let free = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let url = format!("http://{free}/x");
        assert!(send(&Request { method: "GET", url: &url, headers: &[], body: &[] }, 10, Duration::from_millis(500), Duration::from_secs(1), &limits).is_err());
        let bad = [("X", "a\r\nInjected: 1")];
        assert!(send(&Request { method: "GET", url: &url, headers: &bad, body: &[] }, 10, Duration::from_millis(500), Duration::from_secs(1), &limits).is_err());
        assert!(send(&Request { method: "get", url: &url, headers: &[], body: &[] }, 10, Duration::from_millis(500), Duration::from_secs(1), &limits).is_err());
    }
}

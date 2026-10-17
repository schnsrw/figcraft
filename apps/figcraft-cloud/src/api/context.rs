//! Per-request context: request ID, W3C trace context, the structured request log, the error
//! envelope, and the coarse protections every route shares (in-flight cap, deadline, body size).
//!
//! The log records the method, the matched route *template* (never the raw path, query string,
//! cookies or authorization), the status, the duration and the trace identifiers.

use std::convert::Infallible;
use std::io::Write as _;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{FromRequestParts, MatchedPath, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use figcraft_cloud_domain::RequestId;
use tokio::sync::Semaphore;

use super::error::{ApiError, ErrorMarker, envelope};

/// Requests whose header names and values together exceed this are refused with `431` (the
/// handwritten server this replaced capped the whole head at 8 KiB).
pub(crate) const MAX_HEADER_BYTES: usize = 16 * 1024;
use crate::config::CloudRole;

/// The request ID and validated trace context of the current request.
#[derive(Clone, Debug)]
pub(crate) struct RequestContext {
    pub(crate) request_id: RequestId,
    pub(crate) trace: Option<TraceParent>,
}

/// A validated `traceparent` (version 00).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TraceParent {
    pub(crate) value: String,
    pub(crate) trace_id: String,
    pub(crate) parent_id: String,
}

/// Parse a W3C `traceparent` value; anything but a well-formed version-00 value is dropped.
pub(crate) fn parse_traceparent(value: &str) -> Option<TraceParent> {
    let value = value.trim();
    if value.len() != 55 {
        return None;
    }
    let mut parts = value.split('-');
    let version = parts.next()?;
    let trace_id = parts.next()?;
    let parent_id = parts.next()?;
    let flags = parts.next()?;
    let hex = |part: &str| part.bytes().all(|byte| byte.is_ascii_hexdigit());
    if parts.next().is_some()
        || version != "00"
        || trace_id.len() != 32
        || parent_id.len() != 16
        || flags.len() != 2
        || !hex(trace_id)
        || !hex(parent_id)
        || !hex(flags)
        || trace_id.bytes().all(|byte| byte == b'0')
        || parent_id.bytes().all(|byte| byte == b'0')
    {
        return None;
    }
    Some(TraceParent { value: value.to_string(), trace_id: trace_id.to_string(), parent_id: parent_id.to_string() })
}

impl RequestContext {
    fn from_headers(headers: &HeaderMap) -> Self {
        let trace = headers.get("traceparent").and_then(|value| value.to_str().ok()).and_then(parse_traceparent);
        Self { request_id: RequestId::random(), trace }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for RequestContext {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(parts.extensions.get::<Self>().cloned().unwrap_or_else(|| Self::from_headers(&parts.headers)))
    }
}

/// Shared settings for [`middleware`].
#[derive(Clone)]
pub(crate) struct ContextSettings {
    pub(crate) role: CloudRole,
    pub(crate) in_flight: Arc<Semaphore>,
    pub(crate) deadline: Duration,
    pub(crate) max_body_bytes: u64,
}

impl ContextSettings {
    pub(crate) fn new(role: CloudRole, max_in_flight: usize, deadline: Duration, max_body_bytes: u64) -> Self {
        Self { role, in_flight: Arc::new(Semaphore::new(max_in_flight)), deadline, max_body_bytes }
    }
}

/// The outermost middleware of every router this process serves.
pub(crate) async fn middleware(State(settings): State<ContextSettings>, mut request: Request, next: Next) -> Response {
    let started = Instant::now();
    let context = RequestContext::from_headers(request.headers());
    request.extensions_mut().insert(context.clone());
    let method = log_method(request.method());
    let route = request.extensions().get::<MatchedPath>().map_or_else(|| "unmatched".to_string(), |path| path.as_str().to_string());
    let declared_length = request.headers().get(header::CONTENT_LENGTH).and_then(|value| value.to_str().ok()?.parse::<u64>().ok());

    let header_bytes =
        request.headers().iter().fold(0usize, |total, (name, value)| total.saturating_add(name.as_str().len()).saturating_add(value.len()));
    let response = if header_bytes > MAX_HEADER_BYTES {
        ApiError::new(figcraft_cloud_domain::ErrorCode::InvalidRequest).with_status(StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE).into_response()
    } else if declared_length.is_some_and(|length| length > settings.max_body_bytes) {
        ApiError::payload_too_large().into_response()
    } else {
        match settings.in_flight.clone().try_acquire_owned() {
            Err(_) => ApiError::unavailable().into_response(),
            Ok(_permit) => match tokio::time::timeout(settings.deadline, next.run(request)).await {
                Ok(response) => response,
                Err(_) => ApiError::unavailable().into_response(),
            },
        }
    };
    let response = finish(response, &context, &route);
    write_request_log(settings.role, method, &route, response.status().as_u16(), started.elapsed(), &context);
    response
}

/// Fill the envelope, normalize bare framework rejections under `/api/`, and add the headers.
fn finish(response: Response, context: &RequestContext, route: &str) -> Response {
    let marker = response.extensions().get::<ErrorMarker>().copied();
    let mut response = match marker {
        Some(ErrorMarker(code)) => {
            let (mut parts, _) = response.into_parts();
            parts.headers.remove(header::CONTENT_LENGTH);
            parts.headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
            Response::from_parts(parts, Body::from(envelope(code, context.request_id)))
        }
        None if route.starts_with("/api/") && is_bare_rejection(response.status()) => {
            let status = response.status();
            let error = if status == StatusCode::PAYLOAD_TOO_LARGE {
                ApiError::payload_too_large()
            } else {
                ApiError::new(figcraft_cloud_domain::ErrorCode::InvalidRequest).with_status(status)
            };
            let (mut parts, _) = error.into_response().into_parts();
            parts.headers.remove(header::CONTENT_LENGTH);
            Response::from_parts(parts, Body::from(envelope(error.code(), context.request_id)))
        }
        None => response,
    };
    if let Ok(value) = HeaderValue::from_str(&context.request_id.to_string()) {
        response.headers_mut().insert("x-request-id", value);
    }
    if let Some(trace) = &context.trace
        && let Ok(value) = HeaderValue::from_str(&trace.value)
    {
        response.headers_mut().insert("traceparent", value);
    }
    response
}

/// Framework-generated client errors that a handler did not produce as an [`ApiError`].
fn is_bare_rejection(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::BAD_REQUEST
            | StatusCode::METHOD_NOT_ALLOWED
            | StatusCode::PAYLOAD_TOO_LARGE
            | StatusCode::UNSUPPORTED_MEDIA_TYPE
            | StatusCode::UNPROCESSABLE_ENTITY
    )
}

fn log_method(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::HEAD => "HEAD",
        Method::POST => "POST",
        Method::PUT => "PUT",
        Method::PATCH => "PATCH",
        Method::DELETE => "DELETE",
        Method::OPTIONS => "OPTIONS",
        _ => "other",
    }
}

fn write_request_log(role: CloudRole, method: &str, route: &str, status: u16, elapsed: Duration, context: &RequestContext) {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| duration.as_millis());
    let mut record = serde_json::json!({
        "timestamp_unix_ms": timestamp,
        "level": "info",
        "event": "http_request",
        "request_id": context.request_id.to_string(),
        "role": role.as_str(),
        "method": method,
        "route": route,
        "status": status,
        "duration_ms": elapsed.as_millis(),
    });
    if let Some(trace) = &context.trace {
        record["trace_id"] = serde_json::Value::String(trace.trace_id.clone());
        record["parent_span_id"] = serde_json::Value::String(trace.parent_id.clone());
    }
    if let Ok(line) = serde_json::to_string(&record) {
        let _ = writeln!(std::io::stdout().lock(), "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traceparent_validation_matches_w3c_version_00() {
        let valid = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
        let parsed = parse_traceparent(valid).unwrap();
        assert_eq!((parsed.trace_id.as_str(), parsed.parent_id.as_str()), ("4bf92f3577b34da6a3ce929d0e0e4736", "00f067aa0ba902b7"));
        for invalid in [
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e473g-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
            "garbage",
            "",
        ] {
            assert_eq!(parse_traceparent(invalid), None, "{invalid}");
        }
    }
}

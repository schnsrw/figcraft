//! The public error envelope and the disclosure policy (S4-016).
//!
//! Every API failure is an [`ApiError`]: a stable [`ErrorCode`] plus an HTTP status. The body is
//! always `{"error":{"code":…,"message":…,"request_id":…}}` built from
//! [`figcraft_cloud_domain::PublicError`], so the message is fixed by the code and no internal
//! cause, input or identifier can reach the client. Handlers do not need the request ID: the
//! response carries an [`ErrorMarker`] and the request-context middleware writes the envelope.
//!
//! # Disclosure policy
//!
//! `docs/saas/security.md` asks for globally opaque identifiers and normalized not-found and
//! forbidden responses where revealing existence creates risk. The rule every resource route
//! follows (use [`authorize_resource`] or [`denied`]):
//!
//! 1. No valid credential: `401 unauthenticated`, before any lookup, for every route.
//! 2. The resource does not exist, belongs to another tenant, or the caller holds no role on it
//!    that lets them *see* it: `404 not_found`. A caller cannot tell these apart, so identifiers
//!    cannot be probed across tenants or into private workspaces, projects and documents.
//! 3. The caller can see the resource but lacks the permission for this action (a viewer trying
//!    to edit, an editor trying to delete a workspace): `403 forbidden`.
//! 4. Tenant-level administration with no resource identifier (billing, provider keys, support
//!    access) answers `403 forbidden` to members without the sensitive permission: membership of
//!    the caller's own tenant is not a secret to them.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Request};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use figcraft_cloud_domain::{ErrorCode, PublicError, RequestId};
use serde::de::DeserializeOwned;

/// A failed API request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ApiError {
    code: ErrorCode,
    status: StatusCode,
    retry_after_seconds: Option<u64>,
}

/// Left on an error response so the context middleware can write the envelope with the request ID.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ErrorMarker(pub(crate) ErrorCode);

impl ApiError {
    /// The error for `code` with its default status.
    pub(crate) const fn new(code: ErrorCode) -> Self {
        Self { code, status: default_status(code), retry_after_seconds: None }
    }

    pub(crate) const fn not_found() -> Self {
        Self::new(ErrorCode::NotFound)
    }

    pub(crate) const fn forbidden() -> Self {
        Self::new(ErrorCode::Forbidden)
    }

    pub(crate) const fn unauthenticated() -> Self {
        Self::new(ErrorCode::Unauthenticated)
    }

    pub(crate) const fn unavailable() -> Self {
        Self::new(ErrorCode::TemporarilyUnavailable)
    }

    /// `413` with the `invalid_request` code (there is no separate public code for size).
    pub(crate) const fn payload_too_large() -> Self {
        Self { code: ErrorCode::InvalidRequest, status: StatusCode::PAYLOAD_TOO_LARGE, retry_after_seconds: None }
    }

    /// `429 rate_limited` with `Retry-After` (at least one second).
    pub(crate) fn rate_limited(retry_after_seconds: u64) -> Self {
        Self { code: ErrorCode::RateLimited, status: StatusCode::TOO_MANY_REQUESTS, retry_after_seconds: Some(retry_after_seconds.max(1)) }
    }

    /// Override the status for a code (e.g. `405` or `415` with `invalid_request`).
    pub(crate) const fn with_status(self, status: StatusCode) -> Self {
        Self { status, ..self }
    }

    pub(crate) const fn code(&self) -> ErrorCode {
        self.code
    }

    pub(crate) const fn status(&self) -> StatusCode {
        self.status
    }
}

/// The default HTTP status for each public code.
pub(crate) const fn default_status(code: ErrorCode) -> StatusCode {
    match code {
        ErrorCode::InvalidRequest | ErrorCode::InvalidCursor => StatusCode::BAD_REQUEST,
        ErrorCode::Unauthenticated => StatusCode::UNAUTHORIZED,
        ErrorCode::Forbidden => StatusCode::FORBIDDEN,
        ErrorCode::NotFound => StatusCode::NOT_FOUND,
        ErrorCode::Conflict | ErrorCode::ResourceInactive => StatusCode::CONFLICT,
        ErrorCode::PreconditionFailed => StatusCode::PRECONDITION_FAILED,
        ErrorCode::QuotaExceeded | ErrorCode::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        ErrorCode::TemporarilyUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// The JSON envelope for `code` and `request_id`.
pub(crate) fn envelope(code: ErrorCode, request_id: RequestId) -> String {
    #[derive(serde::Serialize)]
    struct Envelope {
        error: PublicError,
    }
    serde_json::to_string(&Envelope { error: PublicError::new(code, request_id) })
        .unwrap_or_else(|_| String::from("{\"error\":{\"code\":\"internal\",\"message\":\"The request could not be completed.\"}}"))
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // Placeholder body (no request ID) in case the context middleware is absent; it is
        // replaced by the full envelope on every server-built router.
        let body = format!("{{\"error\":{{\"code\":\"{}\",\"message\":\"{}\"}}}}", self.code.as_str(), self.code.public_message());
        let mut response = (self.status, [(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))], body).into_response();
        if let Some(seconds) = self.retry_after_seconds
            && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response.extensions_mut().insert(ErrorMarker(self.code));
        response
    }
}

/// Apply the disclosure policy to a looked-up resource: missing or invisible → `404`, visible but
/// not permitted → `403`, otherwise the resource.
pub(crate) fn authorize_resource<T>(resource: Option<T>, can_view: impl FnOnce(&T) -> bool, can_act: impl FnOnce(&T) -> bool) -> Result<T, ApiError> {
    let resource = resource.ok_or(ApiError::not_found())?;
    if !can_view(&resource) {
        return Err(ApiError::not_found());
    }
    if !can_act(&resource) {
        return Err(ApiError::forbidden());
    }
    Ok(resource)
}

/// The refusal for a caller who lacks the requested permission: `403` only when they may see
/// the resource, `404` otherwise.
pub(crate) const fn denied(caller_can_view: bool) -> ApiError {
    if caller_can_view { ApiError::forbidden() } else { ApiError::not_found() }
}

/// `Json<T>` whose rejections are [`ApiError`]s (`413` too large, `415` not JSON, `400` invalid)
/// and never echo the parser's message.
#[derive(Debug)]
pub(crate) struct ApiJson<T>(pub(crate) T);

impl<T, S> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(request, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(JsonRejection::MissingJsonContentType(_)) => {
                Err(ApiError::new(ErrorCode::InvalidRequest).with_status(StatusCode::UNSUPPORTED_MEDIA_TYPE))
            }
            Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => Err(ApiError::payload_too_large()),
            Err(_) => Err(ApiError::new(ErrorCode::InvalidRequest)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_has_an_error_status() {
        for code in [
            ErrorCode::InvalidRequest,
            ErrorCode::Unauthenticated,
            ErrorCode::Forbidden,
            ErrorCode::NotFound,
            ErrorCode::Conflict,
            ErrorCode::PreconditionFailed,
            ErrorCode::ResourceInactive,
            ErrorCode::QuotaExceeded,
            ErrorCode::RateLimited,
            ErrorCode::TemporarilyUnavailable,
            ErrorCode::Internal,
            ErrorCode::InvalidCursor,
        ] {
            let status = default_status(code);
            assert!(status.is_client_error() || status.is_server_error(), "{code}");
        }
    }

    #[test]
    fn envelope_contains_only_code_fixed_message_and_request_id() {
        let id: RequestId = "018f2f35-8dd8-7c25-8f3f-3a728cbd632a".parse().unwrap();
        let body: serde_json::Value = serde_json::from_str(&envelope(ErrorCode::NotFound, id)).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"error": {"code": "not_found", "message": "The requested resource was not found.", "request_id": id.to_string()}})
        );
    }

    #[test]
    fn disclosure_policy_hides_invisible_resources_and_forbids_visible_ones() {
        assert_eq!(authorize_resource(None::<u8>, |_| true, |_| true), Err(ApiError::not_found()));
        assert_eq!(authorize_resource(Some(1u8), |_| false, |_| true), Err(ApiError::not_found()));
        assert_eq!(authorize_resource(Some(1u8), |_| false, |_| false), Err(ApiError::not_found()));
        assert_eq!(authorize_resource(Some(1u8), |_| true, |_| false), Err(ApiError::forbidden()));
        assert_eq!(authorize_resource(Some(1u8), |_| true, |_| true), Ok(1));
        assert_eq!(denied(false).status(), StatusCode::NOT_FOUND);
        assert_eq!(denied(true).status(), StatusCode::FORBIDDEN);
        // Forbidden and not-found differ only where the caller can already see the resource.
        assert_eq!(denied(false), ApiError::not_found());
    }

    #[test]
    fn rate_limited_errors_carry_retry_after() {
        let response = ApiError::rate_limited(0).into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "1");
    }
}

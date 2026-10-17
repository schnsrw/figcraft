//! Router-level tests of the `/api/v1` foundation through the full middleware stack.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{ConnectInfo, Path};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::{get, post};
use figcraft_cloud_domain::{DeploymentMode, PrincipalId, RequestId, Role, TenantContext, TenantId};
use tower::ServiceExt as _;

use super::auth::{AuthFuture, AuthRejection, AuthRequest, Authenticated, Authenticator, CredentialKind, SessionInfo};
use super::client_ip::parse_cidrs;
use super::error::{ApiError, ApiJson, authorize_resource};
use super::rate_limit::RateLimitConfig;
use super::{ApiConfig, AppState, api_router, api_router_with};
use crate::api::context::ContextSettings;
use crate::config::CloudRole;

const PRINCIPAL: &str = "a1a10000-0000-4000-8000-000000000001";
const TENANT: &str = "a1a10000-0000-4000-8000-0000000000aa";

fn state(config: ApiConfig) -> AppState {
    let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://nobody@127.0.0.1:9/none").unwrap();
    let objects = figcraft_cloud_storage::objects::ObjectStore::new(&figcraft_cloud_storage::objects::ObjectStoreSettings {
        endpoint: "http://127.0.0.1:9".to_string(),
        region: "us-east-1".to_string(),
        bucket: "figcraft-test".to_string(),
        access_key: "FCtestkey".to_string(),
        secret_key: "FCtestsecret".to_string(),
        path_style: true,
    })
    .unwrap();
    AppState::new(pool, objects, config)
}

/// Accepts `Authorization: Bearer good`; anything else is unauthenticated.
struct FakeAuthenticator;

impl Authenticator for FakeAuthenticator {
    fn authenticate<'a>(&'a self, request: &'a AuthRequest<'a>) -> AuthFuture<'a> {
        let good = request.headers.get("authorization").is_some_and(|value| value == "Bearer good");
        let suspended = request.headers.get("authorization").is_some_and(|value| value == "Bearer suspended");
        Box::pin(async move {
            if suspended {
                return Err(AuthRejection::Inactive);
            }
            if !good {
                return Err(AuthRejection::Unauthenticated);
            }
            let principal: PrincipalId = PRINCIPAL.parse().map_err(|_| AuthRejection::Unavailable)?;
            let tenant: TenantId = TENANT.parse().map_err(|_| AuthRejection::Unavailable)?;
            Ok(Authenticated {
                principal,
                tenant: TenantContext::new(tenant, principal, DeploymentMode::SingleTenant, Role::Editor, []),
                session: SessionInfo { kind: CredentialKind::BrowserSession, id: "a1a10000-0000-4000-8000-0000000000ff".to_string() },
                grant: super::auth::Grant::Member,
                mutating: false,
            })
        })
    }
}

#[derive(serde::Deserialize)]
struct Echo {
    name: String,
}

/// Visible: `1` (actionable) and `2` (view only); `3` exists but is invisible; others missing.
async fn thing(_caller: Authenticated, Path(id): Path<u32>) -> Result<String, ApiError> {
    let found = (1..=3).contains(&id).then_some(id);
    authorize_resource(found, |id| *id != 3, |id| *id == 1).map(|id| format!("thing {id}"))
}

fn test_routes() -> (Router<AppState>, Router<AppState>) {
    let auth = Router::new().route("/login", post(|| async { "login" })).route("/callback", get(|| async { "callback" }));
    let resources = Router::new()
        .route("/whoami", get(|caller: Authenticated| async move { caller.principal.to_string() }))
        .route("/things/{id}", get(thing))
        .route("/echo", post(|ApiJson(echo): ApiJson<Echo>| async move { echo.name }))
        .route("/raw", post(|body: String| async move { body.len().to_string() }));
    (auth, resources)
}

fn app_with(config: ApiConfig, authenticator: Option<Arc<dyn Authenticator>>) -> Router {
    let max_body = u64::try_from(config.max_body_bytes).unwrap();
    let mut state = state(config);
    if let Some(authenticator) = authenticator {
        state = state.with_authenticator(authenticator);
    }
    let (auth, resources) = test_routes();
    let api = api_router_with(state, auth, resources);
    crate::health::app(CloudRole::Api, true, Some(api), ContextSettings::new(CloudRole::Api, 64, Duration::from_secs(5), max_body))
}

fn app() -> Router {
    app_with(ApiConfig::default(), Some(Arc::new(FakeAuthenticator)))
}

fn from(request: axum::http::request::Builder, peer: &str) -> axum::http::request::Builder {
    let peer: SocketAddr = peer.parse().unwrap();
    request.extension(ConnectInfo(peer))
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, serde_json::Value) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 1 << 16).await.unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, headers, body)
}

fn get_request(path: &str) -> Request<Body> {
    from(Request::get(path), "192.0.2.10:5000").body(Body::empty()).unwrap()
}

fn assert_envelope(headers: &HeaderMap, body: &serde_json::Value, code: &str) {
    let request_id = headers.get("x-request-id").unwrap().to_str().unwrap();
    assert!(request_id.parse::<RequestId>().is_ok());
    let error = body.get("error").unwrap_or_else(|| panic!("no envelope in {body}"));
    assert_eq!(error["code"], code, "{body}");
    assert_eq!(error["request_id"], request_id);
    assert!(error["message"].as_str().is_some_and(|message| !message.is_empty()));
    assert_eq!(error.as_object().unwrap().len(), 3, "{body}");
}

#[tokio::test]
async fn unknown_api_paths_answer_not_found_in_the_envelope() {
    let app = app();
    for path in ["/api/v1/nope", "/api/v1/auth/nope", "/api/v1"] {
        let (status, headers, body) = send(&app, get_request(path)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert_envelope(&headers, &body, "not_found");
    }
    // The transport probe keeps its plain body next to the API router.
    let (status, _, body) = send(&app, get_request("/api/v1/healthz")).await;
    assert_eq!((status, body), (StatusCode::OK, serde_json::json!({"status": "transport_ready"})));
}

#[tokio::test]
async fn wrong_methods_under_the_api_answer_in_the_envelope() {
    let (status, headers, body) = send(&app(), from(Request::delete("/api/v1/whoami"), "192.0.2.10:5000").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_envelope(&headers, &body, "invalid_request");
}

#[tokio::test]
async fn the_default_authenticator_denies_everyone() {
    let app = app_with(ApiConfig::default(), None);
    let request = from(Request::get("/api/v1/whoami"), "192.0.2.10:5000").header("authorization", "Bearer good").body(Body::empty()).unwrap();
    let (status, headers, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_envelope(&headers, &body, "unauthenticated");
}

#[tokio::test]
async fn a_plugged_in_authenticator_resolves_the_caller() {
    let app = app();
    let request = from(Request::get("/api/v1/whoami"), "192.0.2.10:5000").header("authorization", "Bearer good").body(Body::empty()).unwrap();
    let (status, _, body) = send(&app, request).await;
    assert_eq!((status, body), (StatusCode::OK, serde_json::Value::String(PRINCIPAL.to_string())));
    let (status, headers, body) = send(&app, get_request("/api/v1/whoami")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_envelope(&headers, &body, "unauthenticated");
    let request = from(Request::get("/api/v1/whoami"), "192.0.2.10:5000").header("authorization", "Bearer suspended").body(Body::empty()).unwrap();
    let (status, headers, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_envelope(&headers, &body, "resource_inactive");
}

#[tokio::test]
async fn disclosure_policy_hides_invisible_resources_behind_not_found() {
    let app = app();
    let call = |id: u32| {
        from(Request::get(format!("/api/v1/things/{id}")), "192.0.2.10:5000").header("authorization", "Bearer good").body(Body::empty()).unwrap()
    };
    assert_eq!(send(&app, call(1)).await.0, StatusCode::OK);
    let (status, headers, body) = send(&app, call(2)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_envelope(&headers, &body, "forbidden");
    let (invisible_status, invisible_headers, invisible) = send(&app, call(3)).await;
    let (missing_status, _, missing) = send(&app, call(9)).await;
    assert_eq!((invisible_status, missing_status), (StatusCode::NOT_FOUND, StatusCode::NOT_FOUND));
    assert_envelope(&invisible_headers, &invisible, "not_found");
    // An existing-but-invisible resource is indistinguishable from a missing one.
    assert_eq!(invisible["error"]["message"], missing["error"]["message"]);
    // Authentication is checked before any lookup.
    let anonymous = from(Request::get("/api/v1/things/3"), "192.0.2.10:5000").body(Body::empty()).unwrap();
    assert_eq!(send(&app, anonymous).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn json_rejections_and_body_limits_use_fixed_messages() {
    let config = ApiConfig { max_body_bytes: 64, ..ApiConfig::default() };
    let app = app_with(config, Some(Arc::new(FakeAuthenticator)));
    let json = |body: Body| from(Request::post("/api/v1/echo"), "192.0.2.10:5000").header("content-type", "application/json").body(body).unwrap();
    let (status, _, body) = send(&app, json(Body::from("{\"name\":\"ok\"}"))).await;
    assert_eq!((status, body), (StatusCode::OK, serde_json::Value::String("ok".to_string())));

    let (status, headers, body) = send(&app, json(Body::from("{\"name\": SECRET-INPUT"))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_envelope(&headers, &body, "invalid_request");
    assert!(!body.to_string().contains("SECRET-INPUT"));

    let not_json = from(Request::post("/api/v1/echo"), "192.0.2.10:5000").header("content-type", "text/plain").body(Body::from("{}")).unwrap();
    let (status, headers, body) = send(&app, not_json).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_envelope(&headers, &body, "invalid_request");

    // Declared too large: refused before the handler.
    let declared = from(Request::post("/api/v1/raw"), "192.0.2.10:5000").header("content-length", "65").body(Body::from(vec![b'x'; 65])).unwrap();
    let (status, headers, body) = send(&app, declared).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_envelope(&headers, &body, "invalid_request");

    // Undeclared (no Content-Length) and too large: cut off by the extractor's limit.
    let stream = futures_body(vec![b'x'; 200]);
    let streamed = from(Request::post("/api/v1/raw"), "192.0.2.10:5000").body(stream).unwrap();
    let (status, headers, body) = send(&app, streamed).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_envelope(&headers, &body, "invalid_request");
    let small = from(Request::post("/api/v1/raw"), "192.0.2.10:5000").body(futures_body(vec![b'x'; 64])).unwrap();
    assert_eq!(send(&app, small).await.2, serde_json::json!(64));
}

/// A body without a `Content-Length` header (as a chunked upload arrives).
fn futures_body(bytes: Vec<u8>) -> Body {
    Body::from(bytes)
}

fn limited_config(trusted: &str) -> ApiConfig {
    ApiConfig {
        trusted_proxies: parse_cidrs(trusted).unwrap(),
        auth_rate_limit: RateLimitConfig { burst: 2, refill_every: Duration::from_millis(300), max_keys: 100 },
        ..ApiConfig::default()
    }
}

fn login(peer: &str, forwarded: Option<&str>) -> Request<Body> {
    let mut request = from(Request::post("/api/v1/auth/login"), peer);
    if let Some(forwarded) = forwarded {
        request = request.header("x-forwarded-for", forwarded);
    }
    request.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn auth_routes_allow_a_burst_then_429_then_refill() {
    let app = app_with(limited_config(""), None);
    for _ in 0..2 {
        assert_eq!(send(&app, login("198.51.100.7:4000", None)).await.0, StatusCode::OK);
    }
    let (status, headers, body) = send(&app, login("198.51.100.7:4001", None)).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers.get("retry-after").unwrap(), "1");
    assert_envelope(&headers, &body, "rate_limited");
    // Other clients and other auth routes have their own buckets; resource routes are not limited.
    assert_eq!(send(&app, login("198.51.100.8:4000", None)).await.0, StatusCode::OK);
    let callback = from(Request::get("/api/v1/auth/callback"), "198.51.100.7:4000").body(Body::empty()).unwrap();
    assert_eq!(send(&app, callback).await.0, StatusCode::OK);
    for _ in 0..5 {
        assert_eq!(send(&app, from(Request::get("/api/v1/nope"), "198.51.100.7:4000").body(Body::empty()).unwrap()).await.0, StatusCode::NOT_FOUND);
    }
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(send(&app, login("198.51.100.7:4000", None)).await.0, StatusCode::OK);
    assert_eq!(send(&app, login("198.51.100.7:4000", None)).await.0, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn forwarded_for_is_trusted_only_from_configured_proxies() {
    // No trusted proxy: rotating X-Forwarded-For does not escape the limit.
    let app = app_with(limited_config(""), None);
    for index in 0..2 {
        assert_eq!(send(&app, login("203.0.113.9:1", Some(&format!("192.0.2.{index}")))).await.0, StatusCode::OK);
    }
    assert_eq!(send(&app, login("203.0.113.9:1", Some("192.0.2.99"))).await.0, StatusCode::TOO_MANY_REQUESTS);

    // Behind the trusted gateway each forwarded client has its own bucket.
    let app = app_with(limited_config("10.0.0.0/8"), None);
    for _ in 0..2 {
        assert_eq!(send(&app, login("10.0.0.2:1", Some("192.0.2.1"))).await.0, StatusCode::OK);
    }
    assert_eq!(send(&app, login("10.0.0.2:1", Some("192.0.2.1"))).await.0, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(send(&app, login("10.0.0.2:1", Some("192.0.2.2"))).await.0, StatusCode::OK);
    // A client cannot pick its bucket by prepending to the header: the proxy's entry wins.
    assert_eq!(send(&app, login("10.0.0.2:1", Some("192.0.2.77, 192.0.2.1"))).await.0, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn the_production_router_mounts_without_route_conflicts() {
    let router = api_router(state(ApiConfig::default()));
    let app = crate::health::app(CloudRole::All, true, Some(router), ContextSettings::new(CloudRole::All, 8, Duration::from_secs(5), 1024));
    let (status, headers, body) = send(&app, get_request("/api/v1/anything")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_envelope(&headers, &body, "not_found");
    assert_eq!(send(&app, get_request("/healthz")).await.0, StatusCode::OK);
}

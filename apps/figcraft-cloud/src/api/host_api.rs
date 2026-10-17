//! The host API (S6-011, S6-012): server-to-server routes a registered host product uses to
//! create, resolve, archive and restore documents by its own identifiers and to request exports.
//!
//! The contract is the checked-in OpenAPI document `openapi/host-api-v1.json`, served unauthenticated
//! at `GET /api/v1/host/openapi.json`. [`HOST_ROUTES`] is the Rust-side route table; the tests
//! require it to equal the document's paths, methods, operation ids and scopes, so the document
//! cannot drift from the code in either direction.
//!
//! # Versioning
//!
//! The URL prefix `/api/v1` is the major version and `info.version` of the document is the
//! contract version `MAJOR.MINOR.PATCH`. Within `1.x` only additive changes are allowed (new
//! optional fields, new routes); removing or changing a field's meaning needs `/api/v2`.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use figcraft_cloud_auth::embed_token::{self, EmbedError};
use figcraft_cloud_domain::{CloudDocumentId, ErrorCode, PrincipalId, ProjectId, ResourceState, Uuid};
use figcraft_cloud_storage::access_guard::MembershipRequirement;
use figcraft_cloud_storage::exports::{ExportError, ExportFormat, ExportRecord};
use figcraft_cloud_storage::host_mappings::{
    HostMappingError, MappingKind, document_current_seq, external_ids_for_internal, map_external_resource, resolve_external_resource,
};
use figcraft_cloud_storage::host_systems::{HostSystemError, MAX_SYSTEM_ID_CHARS, get_host_system};
use figcraft_cloud_storage::idempotency::{
    RepositoryError as IdempotencyError, ReservationOutcome, complete_idempotency_key, reserve_idempotency_key,
};
use figcraft_cloud_storage::metadata::{DocumentRecord, archive_document, create_document, get_document, restore_document};
use figcraft_cloud_storage::resource_admin::get_project_by_id;
use figcraft_cloud_storage::secret_box::SecretKey;
use figcraft_cloud_storage::service_accounts::Scope;
use figcraft_cloud_storage::sessions;
use figcraft_cloud_storage::webhook_delivery::{
    DeliveryError, DeliveryRecord, DeliveryState, get_delivery, list_attempts, list_deliveries, replay_delivery,
};
use figcraft_cloud_storage::webhooks::{
    NewSecret, WebhookEndpoint, WebhookError, create_webhook_endpoint, delete_webhook_endpoint, get_webhook_endpoint, list_webhook_endpoints,
    rotate_webhook_secret, update_webhook_endpoint,
};
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::AppState;
use super::auth::{Authenticated, CredentialKind};
use super::authenticator::hold_caller_access;
use super::documents_api::{repository_error, valid_name};
use super::error::{ApiError, ApiJson};
use super::host_events::publish_document_event;
use super::rate_limit::{Decision, RateLimitConfig};

/// The OpenAPI document served and checked by the tests.
pub(crate) const OPENAPI_JSON: &str = include_str!("../../openapi/host-api-v1.json");

/// One host route: method, path (relative to `/api/v1`), operation id and the service-account
/// scope it needs (`None`: unauthenticated).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HostRoute {
    pub(crate) method: &'static str,
    pub(crate) path: &'static str,
    pub(crate) operation_id: &'static str,
    pub(crate) scope: Option<&'static str>,
}

const fn route(method: &'static str, path: &'static str, operation_id: &'static str, scope: Option<&'static str>) -> HostRoute {
    HostRoute { method, path, operation_id, scope }
}

/// Every host route.
pub(crate) const HOST_ROUTES: &[HostRoute] = &[
    route("GET", "/host/openapi.json", "getHostOpenApi", None),
    route("POST", "/auth/embed/exchange", "exchangeEmbedToken", None),
    route("POST", "/host/systems/{system_id}/documents", "createHostDocument", Some("documents:write")),
    route("GET", "/host/systems/{system_id}/documents/{external_id}", "resolveHostDocument", Some("documents:read")),
    route("POST", "/host/systems/{system_id}/documents/{external_id}/archive", "archiveHostDocument", Some("documents:write")),
    route("POST", "/host/systems/{system_id}/documents/{external_id}/restore", "restoreHostDocument", Some("documents:write")),
    route("POST", "/host/systems/{system_id}/documents/{external_id}/exports", "requestHostExport", Some("exports:create")),
    route("GET", "/host/systems/{system_id}/exports/{export_id}", "getHostExport", Some("jobs:read")),
    route("GET", "/host/systems/{system_id}/webhooks", "listHostWebhooks", Some("webhooks:manage")),
    route("POST", "/host/systems/{system_id}/webhooks", "createHostWebhook", Some("webhooks:manage")),
    route("GET", "/host/systems/{system_id}/webhooks/{webhook_id}", "getHostWebhook", Some("webhooks:manage")),
    route("PATCH", "/host/systems/{system_id}/webhooks/{webhook_id}", "updateHostWebhook", Some("webhooks:manage")),
    route("DELETE", "/host/systems/{system_id}/webhooks/{webhook_id}", "deleteHostWebhook", Some("webhooks:manage")),
    route("POST", "/host/systems/{system_id}/webhooks/{webhook_id}/rotate-secret", "rotateHostWebhookSecret", Some("webhooks:manage")),
    route("GET", "/host/systems/{system_id}/webhooks/{webhook_id}/deliveries", "listHostWebhookDeliveries", Some("webhooks:manage")),
    route("GET", "/host/systems/{system_id}/webhooks/{webhook_id}/deliveries/{delivery_id}", "getHostWebhookDelivery", Some("webhooks:manage")),
    route(
        "POST",
        "/host/systems/{system_id}/webhooks/{webhook_id}/deliveries/{delivery_id}/replay",
        "replayHostWebhookDelivery",
        Some("webhooks:manage"),
    ),
];

/// The host routes this build serves, relative to `/api/v1`.
pub(crate) fn router() -> Router<AppState> {
    // Named `router`, not `routes`: the S4 permission matrix scans `::routes()` merges for its
    // member-role table, while host routes are service-account routes with their own table
    // ([`HOST_ROUTES`]) and conformance tests.
    Router::new()
        .route("/host/openapi.json", get(openapi))
        .route("/host/systems/{system_id}/documents", post(create_document_handler))
        .route("/host/systems/{system_id}/documents/{external_id}", get(resolve_document_handler))
        .route("/host/systems/{system_id}/documents/{external_id}/archive", post(archive_document_handler))
        .route("/host/systems/{system_id}/documents/{external_id}/restore", post(restore_document_handler))
        .route("/host/systems/{system_id}/documents/{external_id}/exports", post(request_export_handler))
        .route("/host/systems/{system_id}/exports/{export_id}", get(get_export_handler))
        .route("/host/systems/{system_id}/webhooks", get(list_webhooks_handler).post(create_webhook_handler))
        .route(
            "/host/systems/{system_id}/webhooks/{webhook_id}",
            get(get_webhook_handler).patch(update_webhook_handler).delete(delete_webhook_handler),
        )
        .route("/host/systems/{system_id}/webhooks/{webhook_id}/rotate-secret", post(rotate_webhook_secret_handler))
        .route("/host/systems/{system_id}/webhooks/{webhook_id}/deliveries", get(list_deliveries_handler))
        .route("/host/systems/{system_id}/webhooks/{webhook_id}/deliveries/{delivery_id}", get(get_delivery_handler))
        .route("/host/systems/{system_id}/webhooks/{webhook_id}/deliveries/{delivery_id}/replay", post(replay_delivery_handler))
}

/// The embed-token exchange, mounted under `/api/v1/auth` so it shares the auth rate limit.
pub(crate) fn auth_router() -> Router<AppState> {
    Router::new().route("/embed/exchange", post(exchange))
}

async fn openapi() -> Response {
    let mut response = (StatusCode::OK, OPENAPI_JSON).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=300"));
    response
}

// ---------------------------------------------------------------------------------------------
// Shared pieces

/// How long an idempotency key keeps its stored response.
const IDEMPOTENCY_TTL_SECS: i64 = 24 * 60 * 60;
const MIN_IDEMPOTENCY_KEY: usize = 8;
const MAX_IDEMPOTENCY_KEY: usize = 255;
/// Embed exchanges one host may make: a burst, then a steady rate. Per issuer, on top of the
/// per-address auth limit.
pub(crate) const EMBED_LIMIT: RateLimitConfig = RateLimitConfig { burst: 120, refill_every: Duration::from_millis(100), max_keys: 2_048 };

const fn invalid() -> ApiError {
    ApiError::new(ErrorCode::InvalidRequest)
}

fn host_error(error: HostSystemError) -> ApiError {
    match error {
        HostSystemError::NotFound => ApiError::not_found(),
        HostSystemError::InvalidInput | HostSystemError::TooManyKeys => invalid(),
        HostSystemError::Conflict => ApiError::new(ErrorCode::Conflict),
        HostSystemError::Database => ApiError::unavailable(),
    }
}

fn mapping_error(error: HostMappingError) -> ApiError {
    match error {
        HostMappingError::InvalidInput => invalid(),
        HostMappingError::NotFound => ApiError::not_found(),
        HostMappingError::Conflict => ApiError::new(ErrorCode::Conflict),
        HostMappingError::TooMany => ApiError::new(ErrorCode::QuotaExceeded),
        HostMappingError::Database => ApiError::unavailable(),
    }
}

fn export_error(error: ExportError) -> ApiError {
    match error {
        ExportError::NotFound => ApiError::not_found(),
        ExportError::NotAuthorized => ApiError::forbidden(),
        ExportError::DocumentUnavailable => ApiError::new(ErrorCode::ResourceInactive),
        ExportError::SeqAhead | ExportError::InvalidSeq | ExportError::InvalidOptions => invalid(),
        ExportError::TooManyOpenExports => ApiError::new(ErrorCode::QuotaExceeded),
        _ => ApiError::new(ErrorCode::Internal),
    }
}

/// Authorize a host call and open the tenant transaction: a service-account credential with
/// `scope` (browser sessions and share links are refused), then the host must be registered and
/// enabled in the caller's tenant. Anything else about the host is `404`.
async fn begin_host<'a>(state: &'a AppState, caller: &Authenticated, system_id: &str, scope: Scope) -> Result<TenantTransaction<'a>, ApiError> {
    if caller.session.kind != CredentialKind::ServiceAccount {
        return Err(ApiError::forbidden());
    }
    caller.require_scope(scope)?;
    if system_id.is_empty() || system_id.len() > 4 * MAX_SYSTEM_ID_CHARS {
        return Err(ApiError::not_found());
    }
    let mut transaction = begin_tenant_transaction(state.pool(), caller.tenant.tenant_id()).await.map_err(|_| ApiError::unavailable())?;
    hold_caller_access(state, &mut transaction, caller, MembershipRequirement::None).await?;
    match get_host_system(&mut transaction, system_id).await.map_err(host_error)? {
        Some(host) if host.enabled => Ok(transaction),
        _ => Err(ApiError::not_found()),
    }
}

async fn commit(transaction: TenantTransaction<'_>) -> Result<(), ApiError> {
    transaction.commit().await.map_err(|_| ApiError::unavailable())
}

fn json_response(status: StatusCode, body: &Value) -> Response {
    let mut response = (status, Json(body)).into_response();
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// The caller's `Idempotency-Key`, hashed: 8 to 255 printable ASCII characters.
fn idempotency_key(headers: &HeaderMap) -> Result<[u8; 32], ApiError> {
    let value = headers.get("idempotency-key").and_then(|value| value.to_str().ok()).ok_or_else(invalid)?;
    if !(MIN_IDEMPOTENCY_KEY..=MAX_IDEMPOTENCY_KEY).contains(&value.len()) || !value.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
        return Err(invalid());
    }
    Ok(Sha256::digest(value.as_bytes()).into())
}

/// What an idempotent request has to remember between reserving its key and completing it.
struct Idempotent {
    route: String,
    key_hash: [u8; 32],
}

/// Reserve the key inside the work's transaction. `Ok(Err(response))` is a stored response to
/// replay; the same key with another request, or one still in flight, is `409`.
async fn reserve(
    transaction: &mut TenantTransaction<'_>,
    caller: &Authenticated,
    operation: &str,
    system_id: &str,
    headers: &HeaderMap,
    request: &Value,
) -> Result<Result<Idempotent, Response>, ApiError> {
    let key_hash = idempotency_key(headers)?;
    let route = format!("host:{operation}:{system_id}");
    let request_hash = Sha256::digest(serde_json::to_vec(request).map_err(|_| invalid())?);
    match reserve_idempotency_key(transaction, caller.principal, &route, &key_hash, &request_hash, IDEMPOTENCY_TTL_SECS).await {
        Ok(ReservationOutcome::Reserved | ReservationOutcome::TakenOver) => Ok(Ok(Idempotent { route, key_hash })),
        Ok(ReservationOutcome::Replay { response_status, response_body, .. }) => {
            let status =
                u16::try_from(response_status).ok().and_then(|code| StatusCode::from_u16(code).ok()).ok_or(ApiError::new(ErrorCode::Internal))?;
            let mut response = (status, [(header::CONTENT_TYPE, "application/json")], response_body).into_response();
            response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response.headers_mut().insert("idempotent-replay", HeaderValue::from_static("true"));
            Ok(Err(response))
        }
        Ok(ReservationOutcome::InFlight) | Err(IdempotencyError::Conflict) => Err(ApiError::new(ErrorCode::Conflict)),
        Err(_) => Err(ApiError::unavailable()),
    }
}

/// Store the response against the key, then commit the work and the key together.
async fn complete(
    mut transaction: TenantTransaction<'_>,
    caller: &Authenticated,
    idempotent: &Idempotent,
    status: StatusCode,
    body: &Value,
) -> Result<Response, ApiError> {
    let bytes = serde_json::to_vec(body).map_err(|_| ApiError::new(ErrorCode::Internal))?;
    complete_idempotency_key(
        &mut transaction,
        caller.principal,
        &idempotent.route,
        &idempotent.key_hash,
        i16::try_from(status.as_u16()).map_err(|_| ApiError::new(ErrorCode::Internal))?,
        &bytes,
        "application/json",
    )
    .await
    .map_err(|_| ApiError::unavailable())?;
    commit(transaction).await?;
    Ok(json_response(status, body))
}

fn document_json(record: &DocumentRecord, external_id: &str) -> Value {
    json!({
        "id": record.id.to_string(),
        "external_id": external_id,
        "workspace_id": record.workspace_id.to_string(),
        "project_id": record.project_id.to_string(),
        "name": record.name,
        "state": super::documents_api::state_str(record.state),
    })
}

/// The document the host's `external_id` maps to, as it exists in this tenant.
async fn mapped_document(transaction: &mut TenantTransaction<'_>, system_id: &str, external_id: &str) -> Result<DocumentRecord, ApiError> {
    let internal = resolve_external_resource(transaction, system_id, MappingKind::Document, external_id)
        .await
        .map_err(mapping_error)?
        .ok_or(ApiError::not_found())?;
    let id = CloudDocumentId::try_from(internal).map_err(|_| ApiError::not_found())?;
    get_document(transaction, id).await.map_err(repository_error)?.ok_or(ApiError::not_found())
}

async fn audit_host(
    transaction: &mut TenantTransaction<'_>,
    caller: &Authenticated,
    action: &str,
    resource_kind: &str,
    resource: Uuid,
    system_id: &str,
) -> Result<(), ApiError> {
    super::documents_api::audit(transaction, caller, action, resource_kind, resource, json!({"system_id": system_id})).await
}

// ---------------------------------------------------------------------------------------------
// Documents

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateDocumentBody {
    external_id: String,
    external_project_id: String,
    name: String,
}

async fn create_document_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(system_id): Path<String>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<CreateDocumentBody>,
) -> Result<Response, ApiError> {
    let name = valid_name(&body.name)?.to_owned();
    let mut transaction = begin_host(&state, &caller, &system_id, Scope::DocumentsWrite).await?;
    let request = json!({"external_id": body.external_id, "external_project_id": body.external_project_id, "name": name});
    let idempotent = match reserve(&mut transaction, &caller, "create_document", &system_id, &headers, &request).await? {
        Ok(idempotent) => idempotent,
        Err(replay) => return Ok(replay),
    };
    if resolve_external_resource(&mut transaction, &system_id, MappingKind::Document, &body.external_id).await.map_err(mapping_error)?.is_some() {
        return Err(ApiError::new(ErrorCode::Conflict));
    }
    let project_id = resolve_external_resource(&mut transaction, &system_id, MappingKind::Project, &body.external_project_id)
        .await
        .map_err(mapping_error)?
        .and_then(|id| ProjectId::try_from(id).ok())
        .ok_or(ApiError::not_found())?;
    let project = get_project_by_id(&mut transaction, project_id).await.map_err(repository_error)?.ok_or(ApiError::not_found())?;
    if project.state != ResourceState::Active {
        return Err(ApiError::new(ErrorCode::ResourceInactive));
    }
    let record =
        create_document(&mut transaction, CloudDocumentId::random(), project.workspace_id, project_id, &name).await.map_err(repository_error)?;
    map_external_resource(&mut transaction, &system_id, MappingKind::Document, &body.external_id, record.id.as_uuid())
        .await
        .map_err(mapping_error)?;
    audit_host(&mut transaction, &caller, "host.document.create", "document", record.id.as_uuid(), &system_id).await?;
    publish_document_event(&mut transaction, "document.created", &record).await?;
    let view = document_json(&record, &body.external_id);
    complete(transaction, &caller, &idempotent, StatusCode::CREATED, &view).await
}

async fn resolve_document_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((system_id, external_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let mut transaction = begin_host(&state, &caller, &system_id, Scope::DocumentsRead).await?;
    let record = mapped_document(&mut transaction, &system_id, &external_id).await?;
    Ok(json_response(StatusCode::OK, &document_json(&record, &external_id)))
}

/// Archive or restore. Both are idempotent: repeating either is a `200` with the document
/// unchanged and no second audit row.
async fn transition(state: &AppState, caller: &Authenticated, system_id: &str, external_id: &str, archive: bool) -> Result<Response, ApiError> {
    let mut transaction = begin_host(state, caller, system_id, Scope::DocumentsWrite).await?;
    let current = mapped_document(&mut transaction, system_id, external_id).await?;
    let target = if archive { ResourceState::Archived } else { ResourceState::Active };
    let record = if current.state == target {
        current
    } else {
        let changed =
            if archive { archive_document(&mut transaction, current.id).await } else { restore_document(&mut transaction, current.id).await }
                .map_err(repository_error)?
                .ok_or(ApiError::not_found())?;
        if changed.state != target {
            return Err(ApiError::new(ErrorCode::ResourceInactive));
        }
        let action = if archive { "host.document.archive" } else { "host.document.restore" };
        audit_host(&mut transaction, caller, action, "document", changed.id.as_uuid(), system_id).await?;
        publish_document_event(&mut transaction, if archive { "document.archived" } else { "document.restored" }, &changed).await?;
        changed
    };
    let view = document_json(&record, external_id);
    commit(transaction).await?;
    Ok(json_response(StatusCode::OK, &view))
}

async fn archive_document_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(path): Path<(String, String)>,
) -> Result<Response, ApiError> {
    transition(&state, &caller, &path.0, &path.1, true).await
}

async fn restore_document_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(path): Path<(String, String)>,
) -> Result<Response, ApiError> {
    transition(&state, &caller, &path.0, &path.1, false).await
}

// ---------------------------------------------------------------------------------------------
// Exports

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestExportBody {
    format: String,
    external_user_id: String,
}

fn export_json(record: &ExportRecord) -> Value {
    use figcraft_cloud_storage::exports::ExportState;
    let state = match record.state {
        ExportState::Queued => "queued",
        ExportState::Running => "running",
        ExportState::Succeeded => "succeeded",
        ExportState::Failed => "failed",
        ExportState::Expired => "expired",
    };
    json!({
        "id": record.id.to_string(),
        "document_id": record.document_id.to_string(),
        "format": record.format.as_str(),
        "state": state,
        "error_code": record.error_code,
        "completed_at": record.completed_at_unix,
    })
}

async fn request_export_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((system_id, external_id)): Path<(String, String)>,
    headers: HeaderMap,
    ApiJson(body): ApiJson<RequestExportBody>,
) -> Result<Response, ApiError> {
    let format = ExportFormat::parse(&body.format).ok_or_else(invalid)?;
    let mut transaction = begin_host(&state, &caller, &system_id, Scope::ExportsCreate).await?;
    let request = json!({"external_id": external_id, "format": body.format, "external_user_id": body.external_user_id});
    let idempotent = match reserve(&mut transaction, &caller, "request_export", &system_id, &headers, &request).await? {
        Ok(idempotent) => idempotent,
        Err(replay) => return Ok(replay),
    };
    let document = mapped_document(&mut transaction, &system_id, &external_id).await?;
    let requester = resolve_external_resource(&mut transaction, &system_id, MappingKind::Principal, &body.external_user_id)
        .await
        .map_err(mapping_error)?
        .and_then(|id| PrincipalId::try_from(id).ok())
        .ok_or(ApiError::not_found())?;
    let sequence = document_current_seq(&mut transaction, document.id.as_uuid()).await.map_err(mapping_error)?.ok_or(ApiError::not_found())?;
    let record = figcraft_cloud_storage::exports::request_export(&mut transaction, document.id, requester, format, &json!({}), sequence)
        .await
        .map_err(export_error)?;
    audit_host(&mut transaction, &caller, "host.export.request", "export", record.id, &system_id).await?;
    complete(transaction, &caller, &idempotent, StatusCode::ACCEPTED, &export_json(&record)).await
}

async fn get_export_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((system_id, export_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let export_id: Uuid = export_id.parse().map_err(|_| ApiError::not_found())?;
    let mut transaction = begin_host(&state, &caller, &system_id, Scope::JobsRead).await?;
    let record = figcraft_cloud_storage::exports::get_export(&mut transaction, export_id).await.map_err(export_error)?.ok_or(ApiError::not_found())?;
    // Only exports of documents this host has mapped are visible through it.
    let mapped =
        external_ids_for_internal(&mut transaction, &system_id, MappingKind::Document, record.document_id.as_uuid()).await.map_err(mapping_error)?;
    if mapped.is_empty() {
        return Err(ApiError::not_found());
    }
    Ok(json_response(StatusCode::OK, &export_json(&record)))
}

// ---------------------------------------------------------------------------------------------
// Webhook endpoints (S6-013)

fn webhook_error(error: WebhookError) -> ApiError {
    match error {
        WebhookError::InvalidInput => invalid(),
        WebhookError::NotFound => ApiError::not_found(),
        WebhookError::TooMany => ApiError::new(ErrorCode::QuotaExceeded),
        WebhookError::SecretUnavailable | WebhookError::Database => ApiError::unavailable(),
    }
}

/// The key signing secrets are sealed with; without `SESSION_SIGNING_KEY` the feature is off.
fn secret_key(state: &AppState) -> Result<&SecretKey, ApiError> {
    state.identity().and_then(|identity| identity.secret_key()).ok_or_else(ApiError::unavailable)
}

fn webhook_json(endpoint: &WebhookEndpoint) -> Value {
    json!({
        "id": endpoint.id.to_string(),
        "system_id": endpoint.system_id,
        "url": endpoint.url,
        "event_types": endpoint.event_types,
        "enabled": endpoint.enabled,
        "previous_secret_active": endpoint.previous_secret_active,
    })
}

/// A webhook endpoint with the signing secret, which is shown here and never again.
fn webhook_with_secret(endpoint: &WebhookEndpoint, secret: &NewSecret) -> Value {
    let mut body = webhook_json(endpoint);
    body["secret"] = Value::String(secret.expose().to_owned());
    body
}

fn webhook_id(id: &str) -> Result<Uuid, ApiError> {
    id.parse().map_err(|_| ApiError::not_found())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateWebhookBody {
    url: String,
    #[serde(default)]
    event_types: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateWebhookBody {
    url: Option<String>,
    event_types: Option<Vec<String>>,
    enabled: Option<bool>,
}

async fn create_webhook_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(system_id): Path<String>,
    ApiJson(body): ApiJson<CreateWebhookBody>,
) -> Result<Response, ApiError> {
    let mut transaction = begin_host(&state, &caller, &system_id, Scope::WebhooksManage).await?;
    let key = secret_key(&state)?;
    let (endpoint, secret) = create_webhook_endpoint(&mut transaction, key, state.webhook_policy(), &system_id, &body.url, &body.event_types)
        .await
        .map_err(webhook_error)?;
    audit_host(&mut transaction, &caller, "host.webhook.create", "webhook", endpoint.id, &system_id).await?;
    let view = webhook_with_secret(&endpoint, &secret);
    commit(transaction).await?;
    Ok(json_response(StatusCode::CREATED, &view))
}

async fn list_webhooks_handler(State(state): State<AppState>, caller: Authenticated, Path(system_id): Path<String>) -> Result<Response, ApiError> {
    let mut transaction = begin_host(&state, &caller, &system_id, Scope::WebhooksManage).await?;
    let endpoints = list_webhook_endpoints(&mut transaction, &system_id).await.map_err(webhook_error)?;
    Ok(json_response(StatusCode::OK, &json!({"webhooks": endpoints.iter().map(webhook_json).collect::<Vec<_>>()})))
}

async fn get_webhook_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((system_id, id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let id = webhook_id(&id)?;
    let mut transaction = begin_host(&state, &caller, &system_id, Scope::WebhooksManage).await?;
    let endpoint = get_webhook_endpoint(&mut transaction, &system_id, id).await.map_err(webhook_error)?.ok_or(ApiError::not_found())?;
    Ok(json_response(StatusCode::OK, &webhook_json(&endpoint)))
}

async fn update_webhook_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((system_id, id)): Path<(String, String)>,
    ApiJson(body): ApiJson<UpdateWebhookBody>,
) -> Result<Response, ApiError> {
    let id = webhook_id(&id)?;
    let mut transaction = begin_host(&state, &caller, &system_id, Scope::WebhooksManage).await?;
    let endpoint = update_webhook_endpoint(
        &mut transaction,
        state.webhook_policy(),
        &system_id,
        id,
        body.url.as_deref(),
        body.event_types.as_deref(),
        body.enabled,
    )
    .await
    .map_err(webhook_error)?
    .ok_or(ApiError::not_found())?;
    audit_host(&mut transaction, &caller, "host.webhook.update", "webhook", id, &system_id).await?;
    let view = webhook_json(&endpoint);
    commit(transaction).await?;
    Ok(json_response(StatusCode::OK, &view))
}

async fn delete_webhook_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((system_id, id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let id = webhook_id(&id)?;
    let mut transaction = begin_host(&state, &caller, &system_id, Scope::WebhooksManage).await?;
    if !delete_webhook_endpoint(&mut transaction, &system_id, id).await.map_err(webhook_error)? {
        return Err(ApiError::not_found());
    }
    audit_host(&mut transaction, &caller, "host.webhook.delete", "webhook", id, &system_id).await?;
    commit(transaction).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn rotate_webhook_secret_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((system_id, id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let id = webhook_id(&id)?;
    let mut transaction = begin_host(&state, &caller, &system_id, Scope::WebhooksManage).await?;
    let key = secret_key(&state)?;
    let secret = rotate_webhook_secret(&mut transaction, key, &system_id, id).await.map_err(webhook_error)?.ok_or(ApiError::not_found())?;
    let endpoint = get_webhook_endpoint(&mut transaction, &system_id, id).await.map_err(webhook_error)?.ok_or(ApiError::not_found())?;
    audit_host(&mut transaction, &caller, "host.webhook.rotate_secret", "webhook", id, &system_id).await?;
    let view = webhook_with_secret(&endpoint, &secret);
    commit(transaction).await?;
    Ok(json_response(StatusCode::OK, &view))
}

// ---------------------------------------------------------------------------------------------
// Webhook deliveries (S6-014)

fn delivery_error(error: DeliveryError) -> ApiError {
    match error {
        DeliveryError::InvalidInput => invalid(),
        DeliveryError::NotFound => ApiError::not_found(),
        DeliveryError::Database => ApiError::unavailable(),
    }
}

fn delivery_json(record: &DeliveryRecord) -> Value {
    json!({
        "id": record.id.to_string(),
        "endpoint_id": record.endpoint_id.to_string(),
        "event_id": record.event_id.to_string(),
        "event_type": record.event_type,
        "state": record.state.as_str(),
        "attempts": record.attempts,
        "max_attempts": record.max_attempts,
        "last_status": record.last_status,
        "last_error": record.last_error,
        "next_attempt_at": record.next_attempt_at,
        "created_at": record.created_at,
        "completed_at": record.completed_at,
    })
}

#[derive(Debug, Deserialize)]
struct DeliveriesQuery {
    state: Option<String>,
}

/// The endpoint must exist for this host before any delivery under it is visible.
async fn require_endpoint(transaction: &mut TenantTransaction<'_>, system_id: &str, id: Uuid) -> Result<(), ApiError> {
    get_webhook_endpoint(transaction, system_id, id).await.map_err(webhook_error)?.map(|_| ()).ok_or(ApiError::not_found())
}

async fn list_deliveries_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((system_id, id)): Path<(String, String)>,
    Query(query): Query<DeliveriesQuery>,
) -> Result<Response, ApiError> {
    let id = webhook_id(&id)?;
    let filter = match query.state.as_deref() {
        None => None,
        Some("pending") => Some(DeliveryState::Pending),
        Some("succeeded") => Some(DeliveryState::Succeeded),
        Some("failed") => Some(DeliveryState::Failed),
        Some(_) => return Err(invalid()),
    };
    let mut transaction = begin_host(&state, &caller, &system_id, Scope::WebhooksManage).await?;
    require_endpoint(&mut transaction, &system_id, id).await?;
    let deliveries = list_deliveries(&mut transaction, &system_id, id, filter).await.map_err(delivery_error)?;
    Ok(json_response(StatusCode::OK, &json!({"deliveries": deliveries.iter().map(delivery_json).collect::<Vec<_>>()})))
}

async fn get_delivery_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((system_id, id, delivery_id)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    let (id, delivery_id) = (webhook_id(&id)?, webhook_id(&delivery_id)?);
    let mut transaction = begin_host(&state, &caller, &system_id, Scope::WebhooksManage).await?;
    let record = get_delivery(&mut transaction, &system_id, id, delivery_id).await.map_err(delivery_error)?.ok_or(ApiError::not_found())?;
    let attempts = list_attempts(&mut transaction, delivery_id).await.map_err(delivery_error)?;
    let mut view = delivery_json(&record);
    view["attempt_log"] = Value::Array(
        attempts
            .iter()
            .map(|attempt| {
                json!({
                    "attempt": attempt.attempt,
                    "started_at": attempt.started_at,
                    "duration_ms": attempt.duration_ms,
                    "http_status": attempt.http_status,
                    "error": attempt.error,
                })
            })
            .collect(),
    );
    Ok(json_response(StatusCode::OK, &view))
}

/// Replay a finished delivery: the same event, body and event id again. A delivery that is still
/// pending is `409`.
async fn replay_delivery_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((system_id, id, delivery_id)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    let (id, delivery_id) = (webhook_id(&id)?, webhook_id(&delivery_id)?);
    let mut transaction = begin_host(&state, &caller, &system_id, Scope::WebhooksManage).await?;
    if !replay_delivery(&mut transaction, &system_id, id, delivery_id).await.map_err(delivery_error)? {
        return Err(ApiError::new(ErrorCode::Conflict));
    }
    audit_host(&mut transaction, &caller, "host.webhook.replay", "webhook", delivery_id, &system_id).await?;
    let record = get_delivery(&mut transaction, &system_id, id, delivery_id).await.map_err(delivery_error)?.ok_or(ApiError::not_found())?;
    let view = delivery_json(&record);
    commit(transaction).await?;
    Ok(json_response(StatusCode::ACCEPTED, &view))
}

// ---------------------------------------------------------------------------------------------
// Embed-token exchange

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExchangeBody {
    token: String,
}

/// `POST /auth/embed/exchange {"token"}`: the embedded editor trades the host's one-use token for
/// a browser session and the document it opens. Same-origin only (the editor page itself makes the
/// call, never the host page), rate limited per address by the auth router and per issuer here.
/// Every refusal is the same `401`; a valid token outside the host's boundary does not say which
/// part failed. The token is never logged. The session is an embedded session (S6-010): bound to this
/// host, this tenant, this document and the granted role, ended when the host is disabled or
/// the mapping is removed.
async fn exchange(State(state): State<AppState>, headers: HeaderMap, ApiJson(body): ApiJson<ExchangeBody>) -> Result<Response, ApiError> {
    let identity = state.identity().ok_or(ApiError::unavailable())?;
    if !identity.same_origin(&headers) {
        return Err(ApiError::forbidden());
    }
    if body.token.is_empty() || body.token.len() > embed_token::MAX_TOKEN_BYTES {
        return Err(invalid());
    }
    // Per-issuer limit before any database work.
    if let Ok(issuer) = embed_token::token_issuer(&body.token)
        && let Decision::Limited { retry_after } = state.embed_limiter().check_at(None, &format!("embed:{issuer}"), Instant::now())
    {
        return Err(ApiError::rate_limited(retry_after.as_secs()));
    }
    let now = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs())).unwrap_or(0);
    let verified = match embed_token::exchange_embed_token(state.pool(), &body.token, now).await {
        Ok(verified) => verified,
        Err(EmbedError::Refused(_)) => return Err(ApiError::unauthenticated()),
        Err(EmbedError::Unavailable) => return Err(ApiError::unavailable()),
    };
    let principal = PrincipalId::try_from(verified.principal_id).map_err(|_| ApiError::new(ErrorCode::Internal))?;
    let binding = sessions::EmbedBinding {
        tenant_id: verified.tenant_id,
        system_id: verified.system_id.clone(),
        document_id: verified.document_id,
        role: verified.role,
    };
    let created = sessions::create_embedded_session(state.pool(), principal, &binding, identity.limits()).await.map_err(|error| match error {
        sessions::SessionError::Database | sessions::SessionError::Random => ApiError::unavailable(),
        sessions::SessionError::InactivePrincipal => ApiError::new(ErrorCode::ResourceInactive),
        _ => ApiError::unauthenticated(),
    })?;
    let cookie = identity.policy().set_embed_cookie(&created.token, created.absolute_remaining_secs).ok_or(ApiError::new(ErrorCode::Internal))?;
    let mut response = json_response(
        StatusCode::OK,
        &json!({
            "tenant_id": verified.tenant_id.to_string(),
            "workspace_id": verified.workspace_id.to_string(),
            "document_id": verified.document_id.to_string(),
            "role": verified.role.as_str(),
            "system_id": verified.system_id,
        }),
    );
    response.headers_mut().append(header::SET_COOKIE, HeaderValue::from_str(&cookie).map_err(|_| ApiError::new(ErrorCode::Internal))?);
    Ok(response)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::Value;

    use super::{HOST_ROUTES, OPENAPI_JSON};

    fn document() -> Value {
        serde_json::from_str(OPENAPI_JSON).expect("the checked-in OpenAPI document is valid JSON")
    }

    #[tokio::test]
    async fn the_document_is_served_unauthenticated_and_verbatim() {
        use axum::body::{Body, to_bytes};
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt as _;

        let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://nobody@127.0.0.1:9/none").expect("lazy pool");
        let objects = figcraft_cloud_storage::objects::ObjectStore::new(&figcraft_cloud_storage::objects::ObjectStoreSettings {
            endpoint: "http://127.0.0.1:9".to_string(),
            region: "us-east-1".to_string(),
            bucket: "figcraft-test".to_string(),
            access_key: "FCtestkey".to_string(),
            secret_key: "FCtestsecret".to_string(),
            path_style: true,
        })
        .expect("object store");
        let app = crate::api::api_router(crate::api::AppState::new(pool, objects, crate::api::ApiConfig::default()));
        let response = app.oneshot(Request::get("/api/v1/host/openapi.json").body(Body::empty()).expect("request")).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1 << 20).await.expect("body");
        assert_eq!(body, OPENAPI_JSON.as_bytes());
    }

    #[test]
    fn the_document_is_a_versioned_openapi_3_1_contract() {
        let doc = document();
        assert_eq!(doc["openapi"], "3.1.0");
        let version = doc["info"]["version"].as_str().expect("info.version");
        let parts: Vec<&str> = version.split('.').collect();
        assert_eq!(parts.len(), 3, "MAJOR.MINOR.PATCH");
        assert!(parts.iter().all(|part| part.parse::<u32>().is_ok()));
        assert_eq!(parts[0], "1", "the /api/v1 prefix is major version 1");
        assert_eq!(doc["servers"][0]["url"], "/api/v1");
    }

    #[test]
    fn route_table_and_document_agree_in_both_directions() {
        let doc = document();
        let mut from_document: BTreeSet<(String, String, String, Option<String>)> = BTreeSet::new();
        for (path, item) in doc["paths"].as_object().expect("paths") {
            for (method, operation) in item.as_object().expect("path item") {
                let id = operation["operationId"].as_str().expect("operationId").to_owned();
                let scope = operation.get("x-required-scope").and_then(Value::as_str).map(str::to_owned);
                // An operation that opts out of security carries no scope.
                if operation.get("security").is_some_and(|security| security.as_array().is_some_and(Vec::is_empty)) {
                    assert!(scope.is_none(), "{id}");
                } else {
                    assert!(scope.is_some(), "{id} needs x-required-scope");
                }
                from_document.insert((method.to_uppercase(), path.clone(), id, scope));
            }
        }
        let from_table: BTreeSet<(String, String, String, Option<String>)> = HOST_ROUTES
            .iter()
            .map(|route| (route.method.to_owned(), route.path.to_owned(), route.operation_id.to_owned(), route.scope.map(str::to_owned)))
            .collect();
        assert_eq!(from_table.len(), HOST_ROUTES.len(), "duplicate route");
        assert_eq!(from_document, from_table);
    }

    #[test]
    fn scopes_are_on_the_service_account_allowlist_and_ids_are_unique() {
        let allowed: BTreeSet<&str> = figcraft_cloud_storage::service_accounts::Scope::ALL.iter().map(|scope| scope.as_str()).collect();
        for route in HOST_ROUTES {
            if let Some(scope) = route.scope {
                assert!(allowed.contains(scope), "{scope}");
            }
        }
        let ids: BTreeSet<&str> = HOST_ROUTES.iter().map(|route| route.operation_id).collect();
        assert_eq!(ids.len(), HOST_ROUTES.len());
    }

    #[test]
    fn every_reference_in_the_document_resolves() {
        fn walk(value: &Value, root: &Value, seen: &mut usize) {
            match value {
                Value::Object(map) => {
                    if let Some(Value::String(target)) = map.get("$ref") {
                        let pointer = target.strip_prefix('#').expect("local reference");
                        assert!(root.pointer(pointer).is_some(), "dangling {target}");
                        *seen += 1;
                    }
                    map.values().for_each(|inner| walk(inner, root, seen));
                }
                Value::Array(items) => items.iter().for_each(|inner| walk(inner, root, seen)),
                _ => {}
            }
        }
        let doc = document();
        let mut seen = 0;
        walk(&doc, &doc, &mut seen);
        assert!(seen > 10);
    }

    #[test]
    fn every_authenticated_operation_documents_the_auth_failures() {
        let doc = document();
        for (path, item) in doc["paths"].as_object().expect("paths") {
            for (method, operation) in item.as_object().expect("path item") {
                if operation.get("x-required-scope").is_some() {
                    for status in ["401", "403"] {
                        assert!(operation["responses"].get(status).is_some(), "{method} {path} lacks {status}");
                    }
                }
            }
        }
    }
}

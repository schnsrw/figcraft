//! Document images over HTTP (TS-009, SAAS-ADR-017): the bytes of images that documents refer to
//! by reference (`docs/collab-protocol.md`, "Images by reference").
//!
//! * `PUT /api/v1/documents/{document_id}/images/{sha256}`: the body is the image file. The
//!   server checks that it hashes to `{sha256}`, that it is a PNG, JPEG, WebP or GIF within the
//!   size and pixel budgets (`figcraft_cloud_storage::validation`), stores it content-addressed in
//!   the caller's tenant (identical bytes are stored once), and attaches it to the document so it
//!   is not collected while the document exists. `201` for new bytes, `200` when the tenant
//!   already held them. Needs `document.edit`.
//! * `GET` the same path: the bytes, only of an image attached to that document, with a one-year
//!   `immutable` cache lifetime (the URL names the content). Needs `document.join_realtime`.
//! * `GET /api/v1/images/openapi.json`: the contract ([`OPENAPI_JSON`]).
//!
//! Callers prove themselves like they do for the room: a browser's session cookie (same-origin
//! `fetch`), a service account, or the room's own token in `X-Figcraft-Room-Token` (a share-link
//! secret, or a realtime ticket while it lasts), so a desktop app that joined with a link needs
//! nothing else. Every path resolves to a tenant and document before any byte is read, another
//! tenant's document or image reads as not found, and an image is served only for a document it is
//! attached to.
//!
//! Limits (`FIGCRAFT_IMAGE_MAX_BYTES` 100 MiB by default, `FIGCRAFT_IMAGE_TENANT_QUOTA_BYTES`, the
//! upload deadline `FIGCRAFT_IMAGE_UPLOAD_SECONDS`) are enforced before the body is read where the
//! request declares its size, and while it is read otherwise; at most [`MAX_TRANSFERS`] images move
//! at once, the rest get `503` and retry. One request carries one image (no resumable upload yet).
//!
//! The authorization and storage steps sit behind [`ImageGate`] so the HTTP contract is tested
//! without a database; [`PgImageGate`] is the production implementation.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::to_bytes;
use axum::extract::{FromRequestParts, Path, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use figcraft_cloud_domain::{CloudDocumentId, ErrorCode, TenantId};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use super::AppState;
use super::auth::Authenticated;
use super::error::ApiError;

/// The route template of an image upload or download, as the request log shows it.
pub(crate) const IMAGE_ROUTE: &str = "/api/v1/documents/{document_id}/images/{sha256}";

/// The header carrying a room's token (share-link secret or realtime ticket).
pub(crate) const ROOM_TOKEN_HEADER: &str = "x-figcraft-room-token";

/// Most images moving at once on one replica (each is held in memory whole).
pub(crate) const MAX_TRANSFERS: usize = 8;

/// The checked-in OpenAPI contract.
pub(crate) const OPENAPI_JSON: &str = include_str!("../../openapi/images-api-v1.json");

/// The producer recorded with stored images.
const PRODUCER: &str = concat!("figcraft-cloud images ", env!("CARGO_PKG_VERSION"));

/// Whether `method` on the matched route `template` is an image upload (the request log's
/// template): the one place a body may be large.
pub(crate) fn is_upload(method: &Method, template: &str) -> bool {
    *method == Method::PUT && template == IMAGE_ROUTE
}

/// Limits for document images.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ImageSettings {
    /// The largest image taken (bytes).
    pub(crate) max_bytes: u64,
    /// The most verified asset bytes a tenant may hold (0: no quota).
    pub(crate) tenant_quota_bytes: u64,
    /// How long one upload may take (seconds).
    pub(crate) upload_seconds: u64,
}

impl Default for ImageSettings {
    fn default() -> Self {
        Self { max_bytes: 100 << 20, tenant_quota_bytes: 0, upload_seconds: 300 }
    }
}

impl ImageSettings {
    /// The largest request body an upload may declare (the image and a little framing).
    pub(crate) const fn upload_body_limit(&self) -> u64 {
        self.max_bytes.saturating_add(4096)
    }

    pub(crate) const fn upload_deadline(&self) -> Duration {
        Duration::from_secs(self.upload_seconds)
    }
}

/// What a caller may do with one document's images.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ImageAccess {
    pub(crate) tenant: TenantId,
    pub(crate) document: CloudDocumentId,
    pub(crate) edit: bool,
}

/// Why storing or reading an image failed (mapped to fixed public errors by [`failure`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ImageFailure {
    TooLarge,
    Unsupported,
    Invalid,
    Quota,
    Missing,
    Inactive,
    Unavailable,
}

/// A stored image.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredImage {
    pub(crate) size: u64,
    pub(crate) media_type: String,
    /// The tenant already held these bytes.
    pub(crate) reused: bool,
}

/// An image read back.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LoadedImage {
    pub(crate) bytes: Vec<u8>,
    pub(crate) media_type: String,
}

pub(crate) type GateFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// How image requests are authorized and stored.
pub(crate) trait ImageGate: Send + Sync + 'static {
    /// Resolve the request's credentials to a tenant and document, for reading or (`write`) adding
    /// images. A document the caller cannot see is `404`, one they can see but not edit `403`.
    fn authorize<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a mut Parts,
        document: &'a str,
        write: bool,
    ) -> GateFuture<'a, Result<ImageAccess, ApiError>>;

    /// Store `bytes` (already hashed to `sha256`) for the document.
    fn put<'a>(
        &'a self,
        state: &'a AppState,
        access: ImageAccess,
        sha256: [u8; 32],
        bytes: Vec<u8>,
        settings: ImageSettings,
    ) -> GateFuture<'a, Result<StoredImage, ImageFailure>>;

    /// Read the image `sha256` of the document.
    fn get<'a>(&'a self, state: &'a AppState, access: ImageAccess, sha256: [u8; 32]) -> GateFuture<'a, Result<LoadedImage, ImageFailure>>;
}

/// The image gate and the bound on transfers, shared by the routes.
#[derive(Clone)]
pub(crate) struct Images {
    gate: Arc<dyn ImageGate>,
    transfers: Arc<Semaphore>,
}

impl Images {
    pub(crate) fn new(gate: Arc<dyn ImageGate>) -> Self {
        Self { gate, transfers: Arc::new(Semaphore::new(MAX_TRANSFERS)) }
    }

    pub(crate) fn production() -> Self {
        Self::new(Arc::new(PgImageGate))
    }

    /// Take one transfer slot (tests use it to fill them all).
    #[cfg(test)]
    pub(crate) fn acquire_for_test(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.transfers.clone().try_acquire_owned().ok()
    }
}

/// The routes, relative to `/api/v1`.
pub(crate) fn router() -> Router<AppState> {
    Router::new().route("/documents/{document_id}/images/{sha256}", put(put_image).get(get_image)).route("/images/openapi.json", get(openapi))
}

async fn openapi() -> Response {
    ([(header::CONTENT_TYPE, HeaderValue::from_static("application/json"))], OPENAPI_JSON).into_response()
}

/// A lower-case hex SHA-256.
pub(crate) fn parse_sha256(text: &str) -> Option<[u8; 32]> {
    figcraft_cloud_storage::assets::parse_sha256_hex(text)
}

fn invalid() -> ApiError {
    ApiError::new(ErrorCode::InvalidRequest)
}

/// The public error for a failure.
fn failure(failure: ImageFailure) -> ApiError {
    match failure {
        ImageFailure::TooLarge => ApiError::payload_too_large().with_note("image_too_large", "The image is larger than this workspace accepts."),
        ImageFailure::Unsupported => ApiError::new(ErrorCode::InvalidRequest)
            .with_status(StatusCode::UNSUPPORTED_MEDIA_TYPE)
            .with_note("unsupported_image", "Only PNG, JPEG, WebP and GIF images are stored."),
        ImageFailure::Invalid => invalid().with_note("invalid_image", "The file is not a valid image within the size limits."),
        ImageFailure::Quota => ApiError::new(ErrorCode::QuotaExceeded)
            .with_status(StatusCode::INSUFFICIENT_STORAGE)
            .with_note("storage_quota", "This workspace has used its image storage."),
        ImageFailure::Missing => ApiError::not_found(),
        ImageFailure::Inactive => ApiError::new(ErrorCode::ResourceInactive),
        ImageFailure::Unavailable => ApiError::unavailable(),
    }
}

fn declared_length(headers: &HeaderMap) -> Option<u64> {
    headers.get(header::CONTENT_LENGTH)?.to_str().ok()?.parse().ok()
}

/// SHA-256 of `bytes` without holding up the async runtime for a large image.
async fn digest(bytes: Vec<u8>) -> Result<(Vec<u8>, [u8; 32]), ApiError> {
    const INLINE_LIMIT: usize = 256 * 1024;
    if bytes.len() <= INLINE_LIMIT {
        let sum = Sha256::digest(&bytes).into();
        return Ok((bytes, sum));
    }
    tokio::task::spawn_blocking(move || {
        let sum = Sha256::digest(&bytes).into();
        (bytes, sum)
    })
    .await
    .map_err(|_| ApiError::unavailable())
}

async fn put_image(State(state): State<AppState>, Path((document, sha256)): Path<(String, String)>, request: Request) -> Result<Response, ApiError> {
    let images = state.images();
    let settings = state.config().images;
    let (mut parts, body) = request.into_parts();
    // Who you are and what you may do come before anything about the body.
    let access = images.gate.authorize(&state, &mut parts, &document, true).await?;
    let wanted = parse_sha256(&sha256).ok_or_else(invalid)?;
    if declared_length(&parts.headers).is_some_and(|length| length > settings.max_bytes) {
        return Err(failure(ImageFailure::TooLarge));
    }
    let _permit = images.transfers.try_acquire().map_err(|_| ApiError::unavailable())?;
    let limit = usize::try_from(settings.max_bytes).unwrap_or(usize::MAX).saturating_add(1);
    let bytes = to_bytes(body, limit).await.map_err(|_| failure(ImageFailure::TooLarge))?;
    if bytes.is_empty() {
        return Err(invalid());
    }
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > settings.max_bytes {
        return Err(failure(ImageFailure::TooLarge));
    }
    let (bytes, sum) = digest(Vec::from(bytes)).await?;
    if sum != wanted {
        return Err(invalid().with_note("hash_mismatch", "The image does not hash to the address it was sent to."));
    }
    let stored = images.gate.put(&state, access, wanted, bytes, settings).await.map_err(failure)?;
    let status = if stored.reused { StatusCode::OK } else { StatusCode::CREATED };
    let body = serde_json::json!({"sha256": sha256, "size": stored.size, "media_type": stored.media_type, "reused": stored.reused});
    let mut response = (status, axum::Json(body)).into_response();
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn get_image(State(state): State<AppState>, Path((document, sha256)): Path<(String, String)>, request: Request) -> Result<Response, ApiError> {
    let images = state.images();
    let (mut parts, _) = request.into_parts();
    let access = images.gate.authorize(&state, &mut parts, &document, false).await?;
    let wanted = parse_sha256(&sha256).ok_or_else(invalid)?;
    let _permit = images.transfers.try_acquire().map_err(|_| ApiError::unavailable())?;
    let image = images.gate.get(&state, access, wanted).await.map_err(failure)?;
    let media = HeaderValue::from_str(&image.media_type).map_err(|_| ApiError::unavailable())?;
    let mut response = (StatusCode::OK, image.bytes).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, media);
    // The address names the content, so a copy never goes stale; `private`: it needs a credential.
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=31536000, immutable"));
    headers.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    headers.insert("content-security-policy", HeaderValue::from_static("default-src 'none'; sandbox"));
    if let Ok(tag) = HeaderValue::from_str(&format!("\"{sha256}\"")) {
        headers.insert(header::ETAG, tag);
    }
    Ok(response)
}

// ---------------------------------------------------------------------------------------------
// Production: roles and share links from PostgreSQL, bytes in the object store.

/// The production [`ImageGate`].
pub(crate) struct PgImageGate;

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs())
}

/// A room token: a share-link secret or a realtime ticket (valid for its minute).
async fn authorize_room_token(state: &AppState, token: &str, document: &str, write: bool) -> Result<ImageAccess, ApiError> {
    use figcraft_cloud_storage::realtime_tickets::{looks_like_ticket, verify};
    use figcraft_cloud_storage::revocation::{MemberAdmission, dead_member_admissions};
    use figcraft_cloud_storage::share_links::{ShareLinkError, ShareRole, resolve_share_link};

    if token.is_empty() || token.len() > 256 {
        return Err(ApiError::unauthenticated());
    }
    let document: CloudDocumentId = document.parse().map_err(|_| ApiError::not_found())?;
    if looks_like_ticket(token) {
        let key = state.realtime_key().ok_or_else(ApiError::unauthenticated)?;
        let ticket = verify(key, token, document, now_secs()).map_err(|_| ApiError::unauthenticated())?;
        let admission = MemberAdmission { tenant: ticket.tenant, document, principal: ticket.principal, session: ticket.session, edit: ticket.edit };
        match dead_member_admissions(state.pool(), &[admission]).await {
            Ok(dead) if dead.is_empty() => {}
            Ok(_) => return Err(ApiError::unauthenticated()),
            Err(_) => return Err(ApiError::unavailable()),
        }
        if write && !ticket.edit {
            return Err(ApiError::forbidden());
        }
        return Ok(ImageAccess { tenant: ticket.tenant, document, edit: ticket.edit });
    }
    let link = resolve_share_link(state.pool(), token).await.map_err(|error| match error {
        ShareLinkError::Refused => ApiError::unauthenticated(),
        _ => ApiError::unavailable(),
    })?;
    // A link for another document reads like an unknown one.
    if link.document_id != document {
        return Err(ApiError::unauthenticated());
    }
    let edit = link.role == ShareRole::Editor;
    if write && !edit {
        return Err(ApiError::forbidden());
    }
    Ok(ImageAccess { tenant: link.tenant_id, document, edit })
}

async fn authorize_caller(state: &AppState, parts: &mut Parts, document: &str, write: bool) -> Result<ImageAccess, ApiError> {
    use figcraft_cloud_storage::service_accounts::Scope;

    let caller = Authenticated::from_request_parts(parts, state).await?;
    caller.require_scope(if write { Scope::DocumentsWrite } else { Scope::DocumentsRead })?;
    if let Ok(id) = document.parse::<CloudDocumentId>() {
        caller.require_document(id)?;
    }
    let (document, edit) = super::documents_api::authorize_realtime(state, &caller, document).await?;
    if write && !edit {
        return Err(ApiError::forbidden());
    }
    Ok(ImageAccess { tenant: caller.tenant.tenant_id(), document, edit })
}

fn asset_failure(error: figcraft_cloud_storage::assets::AssetError) -> ImageFailure {
    use figcraft_cloud_storage::assets::AssetError;
    match error {
        AssetError::InvalidSize => ImageFailure::TooLarge,
        AssetError::UnsupportedMediaType => ImageFailure::Unsupported,
        AssetError::Invalid(_) => ImageFailure::Invalid,
        AssetError::DocumentNotFound => ImageFailure::Missing,
        AssetError::DocumentUnavailable => ImageFailure::Inactive,
        AssetError::TooManyAssets => ImageFailure::Quota,
        _ => ImageFailure::Unavailable,
    }
}

impl ImageGate for PgImageGate {
    fn authorize<'a>(
        &'a self,
        state: &'a AppState,
        parts: &'a mut Parts,
        document: &'a str,
        write: bool,
    ) -> GateFuture<'a, Result<ImageAccess, ApiError>> {
        Box::pin(async move {
            if let Some(token) = parts.headers.get(ROOM_TOKEN_HEADER) {
                let token = token.to_str().map_err(|_| ApiError::unauthenticated())?;
                return authorize_room_token(state, token, document, write).await;
            }
            authorize_caller(state, parts, document, write).await
        })
    }

    fn put<'a>(
        &'a self,
        state: &'a AppState,
        access: ImageAccess,
        sha256: [u8; 32],
        bytes: Vec<u8>,
        settings: ImageSettings,
    ) -> GateFuture<'a, Result<StoredImage, ImageFailure>> {
        Box::pin(async move {
            use figcraft_cloud_storage::assets::{attach_asset, find_asset, store_asset_bytes_with_limit, tenant_asset_bytes};
            use figcraft_cloud_storage::validation::MediaType;

            let media = match MediaType::sniff(&bytes) {
                Some(media @ (MediaType::Png | MediaType::Jpeg | MediaType::Webp | MediaType::Gif)) => media.as_str(),
                _ => return Err(ImageFailure::Unsupported),
            };
            let size = u64::try_from(bytes.len()).map_err(|_| ImageFailure::TooLarge)?;
            let cap = i64::try_from(settings.max_bytes).map_err(|_| ImageFailure::TooLarge)?;
            let mut transaction =
                figcraft_cloud_storage::begin_tenant_transaction(state.pool(), access.tenant).await.map_err(|_| ImageFailure::Unavailable)?;
            // Bytes the tenant holds already cost nothing; new bytes must fit the quota.
            if settings.tenant_quota_bytes > 0 && find_asset(&mut transaction, &sha256).await.map_err(asset_failure)?.is_none() {
                let used = tenant_asset_bytes(&mut transaction).await.map_err(asset_failure)?;
                if used.saturating_add(size) > settings.tenant_quota_bytes {
                    return Err(ImageFailure::Quota);
                }
            }
            let stored = store_asset_bytes_with_limit(&mut transaction, state.objects(), bytes, media, PRODUCER, cap).await.map_err(asset_failure)?;
            attach_asset(&mut transaction, access.document, stored.object.id).await.map_err(asset_failure)?;
            transaction.commit().await.map_err(|_| ImageFailure::Unavailable)?;
            Ok(StoredImage { size, media_type: media.to_string(), reused: stored.reused })
        })
    }

    fn get<'a>(&'a self, state: &'a AppState, access: ImageAccess, sha256: [u8; 32]) -> GateFuture<'a, Result<LoadedImage, ImageFailure>> {
        Box::pin(async move {
            use figcraft_cloud_storage::assets::read_document_asset;

            let mut transaction =
                figcraft_cloud_storage::begin_tenant_transaction(state.pool(), access.tenant).await.map_err(|_| ImageFailure::Unavailable)?;
            let found = read_document_asset(&mut transaction, state.objects(), access.document, &sha256).await.map_err(asset_failure)?;
            let _ = transaction.rollback().await;
            found.map(|(object, bytes)| LoadedImage { bytes, media_type: object.media_type }).ok_or(ImageFailure::Missing)
        })
    }
}

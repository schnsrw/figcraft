//! TS-009 step 3: the document image routes through the real middleware and router, with an
//! in-memory [`ImageGate`] standing in for PostgreSQL and the object store. No database. The fake
//! gate enforces the same contract the production one does (a tenant sees only its own bytes; an
//! image is served only for a document it is attached to), so these tests pin what the HTTP layer
//! asks of it: who is authorized for what, in what order, and the size, hash and concurrency
//! checks around the body.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Request, StatusCode};
use figcraft_cloud_domain::{CloudDocumentId, TenantId};
use sha2::{Digest, Sha256};
use tower::ServiceExt as _;

use super::images_api::{
    self, GateFuture, ImageAccess, ImageFailure, ImageGate, ImageSettings, LoadedImage, MAX_TRANSFERS, ROOM_TOKEN_HEADER, StoredImage,
};
use crate::api::context::ContextSettings;
use crate::api::error::ApiError;
use crate::api::{ApiConfig, AppState, api_router_with};
use crate::config::CloudRole;

const MIB: usize = 1024 * 1024;
const TENANT_A: &str = "a1a10000-0000-4000-8000-0000000000a1";
const TENANT_B: &str = "a1a10000-0000-4000-8000-0000000000b2";
const DOC_1: &str = "d0c10000-0000-4000-8000-000000000001";
const DOC_2: &str = "d0c10000-0000-4000-8000-000000000002";
const DOC_B: &str = "d0c10000-0000-4000-8000-0000000000b1";

fn tenant(s: &str) -> TenantId {
    s.parse().unwrap()
}

fn document(s: &str) -> CloudDocumentId {
    s.parse().unwrap()
}

/// A PNG-looking file of `n` bytes, different for every `seed`.
fn png(n: usize, seed: u8) -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    v.extend((0..n.saturating_sub(8)).map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed)));
    v
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

type Held = (Vec<u8>, &'static str);

#[derive(Default)]
struct Store {
    /// Bytes by tenant and digest: another tenant's identical bytes are a different entry.
    bytes: BTreeMap<(TenantId, [u8; 32]), Held>,
    attached: BTreeSet<(TenantId, CloudDocumentId, [u8; 32])>,
}

/// Credentials (a bearer, or a room token) mapped to the documents they reach.
struct Fake {
    people: BTreeMap<String, BTreeMap<CloudDocumentId, (TenantId, bool)>>,
    store: Mutex<Store>,
}

impl Fake {
    fn new() -> Arc<Fake> {
        let mut people = BTreeMap::new();
        let ann = BTreeMap::from([(document(DOC_1), (tenant(TENANT_A), true)), (document(DOC_2), (tenant(TENANT_A), true))]);
        people.insert("Bearer ann".to_string(), ann);
        people.insert("Bearer viola".to_string(), BTreeMap::from([(document(DOC_1), (tenant(TENANT_A), false))]));
        people.insert("Bearer bob".to_string(), BTreeMap::from([(document(DOC_B), (tenant(TENANT_B), true))]));
        // Bob's tenant also has a document with DOC_1's id (ids are only unique per tenant here).
        people.insert("Bearer bob-same-id".to_string(), BTreeMap::from([(document(DOC_1), (tenant(TENANT_B), true))]));
        people.insert("link-editor".to_string(), BTreeMap::from([(document(DOC_1), (tenant(TENANT_A), true))]));
        people.insert("link-viewer".to_string(), BTreeMap::from([(document(DOC_1), (tenant(TENANT_A), false))]));
        Arc::new(Fake { people, store: Mutex::new(Store::default()) })
    }

    fn held(&self, tenant_id: &str, bytes: &[u8]) -> bool {
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        self.store.lock().unwrap().bytes.contains_key(&(tenant(tenant_id), digest))
    }
}

impl ImageGate for Fake {
    fn authorize<'a>(
        &'a self,
        _: &'a AppState,
        parts: &'a mut Parts,
        document_id: &'a str,
        write: bool,
    ) -> GateFuture<'a, Result<ImageAccess, ApiError>> {
        Box::pin(async move {
            let credential = match (parts.headers.get(ROOM_TOKEN_HEADER), parts.headers.get("authorization")) {
                (Some(token), _) => token.to_str().unwrap_or_default().to_string(),
                (None, Some(bearer)) => bearer.to_str().unwrap_or_default().to_string(),
                (None, None) => return Err(ApiError::unauthenticated()),
            };
            let reach = self.people.get(&credential).ok_or_else(ApiError::unauthenticated)?;
            let id: CloudDocumentId = document_id.parse().map_err(|_| ApiError::not_found())?;
            let (tenant, edit) = reach.get(&id).copied().ok_or_else(ApiError::not_found)?;
            if write && !edit {
                return Err(ApiError::forbidden());
            }
            Ok(ImageAccess { tenant, document: id, edit })
        })
    }

    fn put<'a>(
        &'a self,
        _: &'a AppState,
        access: ImageAccess,
        sha256: [u8; 32],
        bytes: Vec<u8>,
        settings: ImageSettings,
    ) -> GateFuture<'a, Result<StoredImage, ImageFailure>> {
        Box::pin(async move {
            let media = if bytes.starts_with(b"\x89PNG") { "image/png" } else { return Err(ImageFailure::Unsupported) };
            let mut store = self.store.lock().unwrap();
            let size = bytes.len() as u64;
            let reused = store.bytes.contains_key(&(access.tenant, sha256));
            if !reused && settings.tenant_quota_bytes > 0 {
                let used: u64 = store.bytes.iter().filter(|((t, _), _)| *t == access.tenant).map(|(_, (b, _))| b.len() as u64).sum();
                if used + size > settings.tenant_quota_bytes {
                    return Err(ImageFailure::Quota);
                }
            }
            store.bytes.entry((access.tenant, sha256)).or_insert((bytes, media));
            store.attached.insert((access.tenant, access.document, sha256));
            Ok(StoredImage { size, media_type: media.to_string(), reused })
        })
    }

    fn get<'a>(&'a self, _: &'a AppState, access: ImageAccess, sha256: [u8; 32]) -> GateFuture<'a, Result<LoadedImage, ImageFailure>> {
        Box::pin(async move {
            let store = self.store.lock().unwrap();
            if !store.attached.contains(&(access.tenant, access.document, sha256)) {
                return Err(ImageFailure::Missing);
            }
            let (bytes, media) = store.bytes.get(&(access.tenant, sha256)).ok_or(ImageFailure::Missing)?;
            Ok(LoadedImage { bytes: bytes.clone(), media_type: (*media).to_string() })
        })
    }
}

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

fn settings() -> ImageSettings {
    ImageSettings { max_bytes: 100 * MIB as u64, tenant_quota_bytes: 0, upload_seconds: 60 }
}

fn app_with(gate: Arc<Fake>, images: ImageSettings) -> Router {
    // Every other route's body is capped at 1 MiB; uploads get the image limit.
    let config = ApiConfig { max_body_bytes: MIB, images, ..ApiConfig::default() };
    let api = api_router_with(state(config).with_image_gate(gate), Router::new(), images_api::router());
    let context = ContextSettings::new(CloudRole::Api, 64, Duration::from_secs(30), MIB as u64)
        .with_uploads(images.upload_body_limit(), images.upload_deadline());
    crate::health::app(CloudRole::Api, true, Some(api), context)
}

fn app(gate: Arc<Fake>) -> Router {
    app_with(gate, settings())
}

fn put(document_id: &str, hash: &str, who: &str, body: Vec<u8>) -> Request<Body> {
    let mut builder = Request::put(format!("/api/v1/documents/{document_id}/images/{hash}")).header("content-length", body.len().to_string());
    builder = if who.starts_with("Bearer") { builder.header("authorization", who) } else { builder.header(ROOM_TOKEN_HEADER, who) };
    builder.body(Body::from(body)).unwrap()
}

fn get(document_id: &str, hash: &str, who: &str) -> Request<Body> {
    let mut builder = Request::get(format!("/api/v1/documents/{document_id}/images/{hash}"));
    builder = if who.starts_with("Bearer") { builder.header("authorization", who) } else { builder.header(ROOM_TOKEN_HEADER, who) };
    builder.body(Body::empty()).unwrap()
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 200 * MIB).await.unwrap().to_vec();
    (status, headers, bytes)
}

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(bytes).into_owned()))
}

fn reason(bytes: &[u8]) -> String {
    json(bytes)["error"]["reason"].as_str().unwrap_or_default().to_string()
}

// ---- the happy path ------------------------------------------------------------------------

#[tokio::test]
async fn a_thirty_megabyte_image_goes_up_and_comes_back() {
    let gate = Fake::new();
    let app = app(gate.clone());
    let image = png(30 * MIB, 1);
    let hash = hex(&image);
    let (status, headers, body) = send(&app, put(DOC_1, &hash, "Bearer ann", image.clone())).await;
    assert_eq!(status, StatusCode::CREATED, "{}", String::from_utf8_lossy(&body));
    assert_eq!(json(&body), serde_json::json!({"sha256": hash, "size": 30 * MIB, "media_type": "image/png", "reused": false}));
    assert_eq!(headers["cache-control"], "no-store");
    // The same bytes again: nothing new is written.
    let (status, _, body) = send(&app, put(DOC_1, &hash, "Bearer ann", image.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json(&body)["reused"], true);
    // Any reader of the document gets exactly the bytes, as an immutable private copy.
    let (status, headers, body) = send(&app, get(DOC_1, &hash, "Bearer viola")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, image);
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(headers["cache-control"], "private, max-age=31536000, immutable");
    assert_eq!(headers["etag"], format!("\"{hash}\""));
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert!(headers["content-security-policy"].to_str().unwrap().contains("sandbox"));
}

#[tokio::test]
async fn uploads_may_be_big_but_nothing_else_may() {
    // The context middleware caps every body at 1 MiB; only the image PUT is exempt.
    let gate = Fake::new();
    let app = app(gate);
    let image = png(3 * MIB, 2);
    assert_eq!(send(&app, put(DOC_1, &hex(&image), "Bearer ann", image)).await.0, StatusCode::CREATED);
    let big = Request::post("/api/v1/documents/x/other").header("content-length", (2 * MIB).to_string()).body(Body::empty()).unwrap();
    assert_eq!(send(&app, big).await.0, StatusCode::PAYLOAD_TOO_LARGE);
    // And a GET of an image is an ordinary request.
    let hash = hex(b"nothing");
    assert_eq!(send(&app, get(DOC_1, &hash, "Bearer ann")).await.0, StatusCode::NOT_FOUND);
}

// ---- who may do what -----------------------------------------------------------------------

#[tokio::test]
async fn credentials_come_before_the_body_and_the_roles_decide() {
    let gate = Fake::new();
    let app = app(gate.clone());
    let image = png(2 * MIB, 3);
    let hash = hex(&image);
    // No credential, an unknown one: 401, and nothing is stored.
    let anonymous = Request::put(format!("/api/v1/documents/{DOC_1}/images/{hash}")).body(Body::from(image.clone())).unwrap();
    assert_eq!(send(&app, anonymous).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(send(&app, put(DOC_1, &hash, "Bearer mallory", image.clone())).await.0, StatusCode::UNAUTHORIZED);
    // A viewer (or a viewer link) reads but does not add.
    assert_eq!(send(&app, put(DOC_1, &hash, "Bearer viola", image.clone())).await.0, StatusCode::FORBIDDEN);
    assert_eq!(send(&app, put(DOC_1, &hash, "link-viewer", image.clone())).await.0, StatusCode::FORBIDDEN);
    assert!(!gate.held(TENANT_A, &image));
    // An editor link does.
    assert_eq!(send(&app, put(DOC_1, &hash, "link-editor", image.clone())).await.0, StatusCode::CREATED);
    for reader in ["Bearer viola", "link-viewer", "link-editor", "Bearer ann"] {
        assert_eq!(send(&app, get(DOC_1, &hash, reader)).await.0, StatusCode::OK, "{reader}");
    }
    // A document the credential does not reach reads as missing.
    assert_eq!(send(&app, get(DOC_2, &hash, "Bearer viola")).await.0, StatusCode::NOT_FOUND);
    assert_eq!(send(&app, get("not-a-uuid", &hash, "Bearer ann")).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn another_tenant_cannot_read_or_reuse_the_bytes() {
    let gate = Fake::new();
    let app = app(gate.clone());
    let image = png(2 * MIB, 4);
    let hash = hex(&image);
    assert_eq!(send(&app, put(DOC_1, &hash, "Bearer ann", image.clone())).await.0, StatusCode::CREATED);
    // Bob, in another tenant, with his own document: nothing at that hash, in his or in ann's.
    assert_eq!(send(&app, get(DOC_B, &hash, "Bearer bob")).await.0, StatusCode::NOT_FOUND);
    assert_eq!(send(&app, get(DOC_1, &hash, "Bearer bob")).await.0, StatusCode::NOT_FOUND, "ann's document id is not reachable by bob");
    // Even a document with the same id in his tenant holds nothing of ann's.
    assert_eq!(send(&app, get(DOC_1, &hash, "Bearer bob-same-id")).await.0, StatusCode::NOT_FOUND);
    // His upload of the same bytes is new bytes in his tenant, not a reuse of hers.
    let (status, _, body) = send(&app, put(DOC_B, &hash, "Bearer bob", image.clone())).await;
    assert_eq!((status, json(&body)["reused"].clone()), (StatusCode::CREATED, serde_json::json!(false)));
    assert!(gate.held(TENANT_B, &image));
    assert_eq!(send(&app, get(DOC_B, &hash, "Bearer bob")).await.2, image);
}

#[tokio::test]
async fn an_image_is_served_only_for_a_document_it_is_attached_to() {
    let gate = Fake::new();
    let app = app(gate);
    let image = png(2 * MIB, 5);
    let hash = hex(&image);
    assert_eq!(send(&app, put(DOC_1, &hash, "Bearer ann", image.clone())).await.0, StatusCode::CREATED);
    // Ann reaches both documents, but only document 1 holds the image (knowing a hash is not
    // enough to read a document's image through another document).
    assert_eq!(send(&app, get(DOC_2, &hash, "Bearer ann")).await.0, StatusCode::NOT_FOUND);
    // Uploading it for document 2 reuses the bytes and attaches them.
    let (status, _, body) = send(&app, put(DOC_2, &hash, "Bearer ann", image.clone())).await;
    assert_eq!((status, json(&body)["reused"].clone()), (StatusCode::OK, serde_json::json!(true)));
    assert_eq!(send(&app, get(DOC_2, &hash, "Bearer ann")).await.2, image);
}

// ---- the body ------------------------------------------------------------------------------

#[tokio::test]
async fn the_address_must_match_the_bytes_and_be_well_formed() {
    let gate = Fake::new();
    let app = app(gate.clone());
    let image = png(MIB, 6);
    let other = hex(&png(MIB, 7));
    let (status, _, body) = send(&app, put(DOC_1, &other, "Bearer ann", image.clone())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(reason(&body), "hash_mismatch");
    assert!(!gate.held(TENANT_A, &image));
    for bad in ["short", &hex(&image).to_uppercase(), &format!("{}0", hex(&image)), "../../etc/passwd"] {
        let (status, _, _) = send(&app, put(DOC_1, bad, "Bearer ann", image.clone())).await;
        assert!(status == StatusCode::BAD_REQUEST || status == StatusCode::NOT_FOUND, "{bad}: {status}");
        assert_ne!(send(&app, get(DOC_1, bad, "Bearer ann")).await.0, StatusCode::OK, "{bad}");
    }
    // An empty body is not an image.
    assert_eq!(send(&app, put(DOC_1, &hex(b""), "Bearer ann", Vec::new())).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn only_images_are_stored() {
    let app = app(Fake::new());
    let text = b"<svg onload=alert(1)></svg>".to_vec();
    let (status, _, body) = send(&app, put(DOC_1, &hex(&text), "Bearer ann", text)).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(reason(&body), "unsupported_image");
}

#[tokio::test]
async fn the_size_limit_is_enforced_before_and_while_the_body_is_read() {
    let limits = ImageSettings { max_bytes: 4 * MIB as u64, ..settings() };
    let gate = Fake::new();
    let app = app_with(gate.clone(), limits);
    let big = png(5 * MIB, 8);
    let hash = hex(&big);
    // Declared far too large: refused from the header by the middleware, before any handler.
    assert_eq!(send(&app, put(DOC_1, &hash, "Bearer ann", big.clone())).await.0, StatusCode::PAYLOAD_TOO_LARGE);
    // Declared a byte over: the handler refuses it from the header, with its reason.
    let one_over = png(4 * MIB + 1, 14);
    let (status, _, body) = send(&app, put(DOC_1, &hex(&one_over), "Bearer ann", one_over)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(reason(&body), "image_too_large");
    // Undeclared (chunked): refused as it is read.
    let undeclared =
        Request::put(format!("/api/v1/documents/{DOC_1}/images/{hash}")).header("authorization", "Bearer ann").body(Body::from(big.clone())).unwrap();
    assert_eq!(send(&app, undeclared).await.0, StatusCode::PAYLOAD_TOO_LARGE);
    // A lie about the size does not get past the check either.
    let lie = Request::put(format!("/api/v1/documents/{DOC_1}/images/{hash}"))
        .header("authorization", "Bearer ann")
        .header("content-length", "100")
        .body(Body::from(big.clone()))
        .unwrap();
    let status = send(&app, lie).await.0;
    assert!(status.is_client_error(), "{status}");
    assert!(!gate.held(TENANT_A, &big));
    // At the limit it is fine.
    let fits = png(4 * MIB, 9);
    assert_eq!(send(&app, put(DOC_1, &hex(&fits), "Bearer ann", fits)).await.0, StatusCode::CREATED);
}

#[tokio::test]
async fn the_tenant_quota_stops_new_bytes_but_not_bytes_it_already_holds() {
    let limits = ImageSettings { tenant_quota_bytes: 5 * MIB as u64, ..settings() };
    let app = app_with(Fake::new(), limits);
    let first = png(3 * MIB, 10);
    let second = png(3 * MIB, 11);
    assert_eq!(send(&app, put(DOC_1, &hex(&first), "Bearer ann", first.clone())).await.0, StatusCode::CREATED);
    let (status, _, body) = send(&app, put(DOC_1, &hex(&second), "Bearer ann", second)).await;
    assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(reason(&body), "storage_quota");
    // The same bytes for another document are not new bytes.
    assert_eq!(send(&app, put(DOC_2, &hex(&first), "Bearer ann", first)).await.0, StatusCode::OK);
    // Another tenant has its own quota.
    let theirs = png(3 * MIB, 12);
    assert_eq!(send(&app, put(DOC_B, &hex(&theirs), "Bearer bob", theirs)).await.0, StatusCode::CREATED);
}

#[tokio::test]
async fn transfers_in_flight_are_bounded() {
    let gate = Fake::new();
    let state = state(ApiConfig { max_body_bytes: MIB, images: settings(), ..ApiConfig::default() }).with_image_gate(gate);
    // Hold every permit: the next transfer is turned away, not queued.
    let held: Vec<_> = (0..MAX_TRANSFERS).map(|_| state.images().acquire_for_test()).collect();
    assert!(held.iter().all(Option::is_some));
    let api = api_router_with(state, Router::new(), images_api::router());
    let context =
        ContextSettings::new(CloudRole::Api, 64, Duration::from_secs(30), MIB as u64).with_uploads(200 * MIB as u64, Duration::from_secs(60));
    let app = crate::health::app(CloudRole::Api, true, Some(api), context);
    let image = png(MIB, 13);
    assert_eq!(send(&app, put(DOC_1, &hex(&image), "Bearer ann", image)).await.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(send(&app, get(DOC_1, &hex(b"x"), "Bearer ann")).await.0, StatusCode::SERVICE_UNAVAILABLE);
    drop(held);
}

// ---- the contract --------------------------------------------------------------------------

#[test]
fn only_the_image_put_is_an_upload() {
    use axum::http::Method;
    assert!(images_api::is_upload(&Method::PUT, images_api::IMAGE_ROUTE));
    assert!(!images_api::is_upload(&Method::GET, images_api::IMAGE_ROUTE));
    assert!(!images_api::is_upload(&Method::POST, images_api::IMAGE_ROUTE));
    assert!(!images_api::is_upload(&Method::PUT, "/api/v1/documents/{document_id}/members/{principal_id}"));
    assert_eq!(images_api::parse_sha256(&hex(b"a")).map(|h| h[0]), Some(Sha256::digest(b"a")[0]));
    assert!(images_api::parse_sha256("xyz").is_none());
    assert!(images_api::parse_sha256(&"g".repeat(64)).is_none());
}

#[tokio::test]
async fn the_openapi_document_matches_the_routes() {
    let app = app(Fake::new());
    let (status, headers, body) = send(&app, Request::get("/api/v1/images/openapi.json").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/json");
    let doc = json(&body);
    assert_eq!(doc["openapi"], "3.1.0");
    let path = &doc["paths"]["/documents/{document_id}/images/{sha256}"];
    assert_eq!(path["put"]["operationId"], "putDocumentImage");
    assert_eq!(path["get"]["operationId"], "getDocumentImage");
    // Every status the handlers can answer is documented.
    for code in ["200", "201", "400", "401", "403", "404", "413", "415", "503", "507"] {
        assert!(path["put"]["responses"].get(code).is_some(), "PUT {code}");
    }
    // The documented path is the route template the middleware recognises uploads by.
    assert_eq!(format!("/api/v1{}", "/documents/{document_id}/images/{sha256}"), images_api::IMAGE_ROUTE);
}

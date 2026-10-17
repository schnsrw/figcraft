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

use axum::Router;
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use super::AppState;

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
    route("POST", "/host/systems/{system_id}/documents", "createHostDocument", Some("documents:write")),
    route("GET", "/host/systems/{system_id}/documents/{external_id}", "resolveHostDocument", Some("documents:read")),
    route("POST", "/host/systems/{system_id}/documents/{external_id}/archive", "archiveHostDocument", Some("documents:write")),
    route("POST", "/host/systems/{system_id}/documents/{external_id}/restore", "restoreHostDocument", Some("documents:write")),
    route("POST", "/host/systems/{system_id}/documents/{external_id}/exports", "requestHostExport", Some("exports:create")),
    route("GET", "/host/systems/{system_id}/exports/{export_id}", "getHostExport", Some("jobs:read")),
];

/// The host routes this build serves, relative to `/api/v1`.
pub(crate) fn router() -> Router<AppState> {
    // Named `router`, not `routes`: the S4 permission matrix scans `::routes()` merges for its
    // member-role table, while host routes are service-account routes with their own table
    // ([`HOST_ROUTES`]) and conformance tests.
    Router::new().route("/host/openapi.json", get(openapi))
}

async fn openapi() -> Response {
    let mut response = (axum::http::StatusCode::OK, OPENAPI_JSON).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=300"));
    response
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

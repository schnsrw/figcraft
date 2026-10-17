//! The OpenAPI contract of the built-in accounts, history and versions routes (S6-034 to S6-043),
//! kept in step with the code by tests (the same discipline as the host API's document).
//!
//! The checked-in document is `openapi/accounts-api-v1.json`, served unauthenticated at
//! `GET /api/v1/auth/openapi.json`. [`ACCOUNT_ROUTES`] is the Rust-side route table: the tests fail
//! when the document and the table disagree in either direction, when an operation id repeats, or
//! when a documented authorization is not one of the known forms. Every route added by the account
//! workstream goes into both.

use axum::Router;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use super::AppState;

pub(crate) const OPENAPI_JSON: &str = include_str!("../../openapi/accounts-api-v1.json");

/// One documented route: method, path (relative to `/api/v1`), operation id and who may call it
/// (`none`, `session`, or the tenant permission an authenticated member needs).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AccountRoute {
    pub(crate) method: &'static str,
    pub(crate) path: &'static str,
    pub(crate) operation_id: &'static str,
    pub(crate) auth: &'static str,
}

const fn route(method: &'static str, path: &'static str, operation_id: &'static str, auth: &'static str) -> AccountRoute {
    AccountRoute { method, path, operation_id, auth }
}

/// Every documented account route.
pub(crate) const ACCOUNT_ROUTES: &[AccountRoute] = &[
    route("GET", "/auth/methods", "getSignInMethods", "none"),
    route("POST", "/auth/password/signup", "passwordSignUp", "none"),
    route("POST", "/auth/password/bootstrap", "passwordBootstrap", "none"),
    route("POST", "/auth/password/login", "passwordLogin", "none"),
    route("GET", "/tenants/{tenant}/users", "listUsers", "member.view"),
    route("POST", "/tenants/{tenant}/users", "createUser", "member.manage"),
    route("POST", "/tenants/{tenant}/users/{principal}/disable", "disableUser", "member.manage"),
    route("POST", "/tenants/{tenant}/users/{principal}/enable", "enableUser", "member.manage"),
    route("POST", "/tenants/{tenant}/users/{principal}/reset-link", "issueResetLink", "member.manage"),
    route("POST", "/tenants/{tenant}/users/{principal}/revoke-sessions", "revokeUserSessions", "member.manage"),
    route("POST", "/auth/password/reset", "resetPassword", "none"),
    route("GET", "/me", "getMe", "session"),
    route("PATCH", "/me", "updateMe", "session"),
    route("POST", "/me/password", "changePassword", "session"),
    route("GET", "/me/sessions", "listMySessions", "session"),
    route("DELETE", "/me/sessions/{id}", "revokeMySession", "session"),
    route("POST", "/me/sessions/revoke-others", "revokeOtherSessions", "session"),
    route("GET", "/tenants/{tenant}/account-settings", "getAccountSettings", "tenant.view"),
    route("PUT", "/tenants/{tenant}/account-settings", "putAccountSettings", "tenant.update_policy"),
];

/// Routes under `/api/v1/auth`.
pub(crate) fn routes() -> Router<AppState> {
    Router::new().route("/openapi.json", get(openapi))
}

async fn openapi() -> Response {
    let mut response = (StatusCode::OK, OPENAPI_JSON).into_response();
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=300"));
    response
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::Value;

    use super::*;

    fn document() -> Value {
        serde_json::from_str(OPENAPI_JSON).expect("the document is JSON")
    }

    #[test]
    fn the_document_is_a_versioned_openapi_3_1_contract() {
        let doc = document();
        assert_eq!(doc["openapi"], "3.1.0");
        assert_eq!(doc["servers"][0]["url"], "/api/v1");
        let version = doc["info"]["version"].as_str().expect("version");
        assert_eq!(version.split('.').count(), 3);
    }

    #[test]
    fn route_table_and_document_agree_in_both_directions() {
        let doc = document();
        let mut from_document = BTreeSet::new();
        for (path, item) in doc["paths"].as_object().expect("paths") {
            for (method, operation) in item.as_object().expect("item") {
                let id = operation["operationId"].as_str().expect("operationId").to_owned();
                let auth = operation["x-auth"].as_str().expect("x-auth").to_owned();
                from_document.insert((method.to_uppercase(), path.clone(), id, auth));
            }
        }
        let from_table: BTreeSet<_> =
            ACCOUNT_ROUTES.iter().map(|r| (r.method.to_owned(), r.path.to_owned(), r.operation_id.to_owned(), r.auth.to_owned())).collect();
        assert_eq!(from_table.len(), ACCOUNT_ROUTES.len(), "duplicate route");
        assert_eq!(from_document, from_table);
        let ids: BTreeSet<_> = ACCOUNT_ROUTES.iter().map(|r| r.operation_id).collect();
        assert_eq!(ids.len(), ACCOUNT_ROUTES.len(), "operation ids are unique");
    }

    #[test]
    fn documented_permissions_are_known_actions() {
        let actions: BTreeSet<&str> = figcraft_cloud_auth::authz::Action::ALL.iter().map(|action| action.id()).collect();
        for route in ACCOUNT_ROUTES {
            assert!(matches!(route.auth, "none" | "session") || actions.contains(route.auth), "{} {}", route.method, route.path);
        }
    }

    #[test]
    fn every_response_and_body_reference_resolves() {
        let doc = document();
        let schemas = doc["components"]["schemas"].as_object().expect("schemas");
        let text = OPENAPI_JSON;
        let mut rest = text;
        while let Some(at) = rest.find("#/components/schemas/") {
            let tail = &rest[at + "#/components/schemas/".len()..];
            let name: String = tail.chars().take_while(|c| c.is_alphanumeric()).collect();
            assert!(schemas.contains_key(&name), "dangling reference {name}");
            rest = tail;
        }
    }
}

//! The signed-in person's own view, before any workspace is chosen (S6-047).
//!
//! `GET /api/v1/tenants` needs a workspace context, which a person who has just signed in does not
//! have yet. `GET /api/v1/me/workspaces` lists the workspaces the caller belongs to with their
//! lifecycle state and their role, so the workspace app can offer a picker and explain a workspace
//! that is suspended, archived or pending deletion. It needs only a browser session.

use axum::Json;
use axum::Router;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use figcraft_cloud_storage::metadata::PageSize;
use figcraft_cloud_storage::tenants;

use super::AppState;
use super::auth::CredentialKind;
use super::authenticator::SignedIn;
use super::cursor::{CursorScope, ListQuery, PageBody, PageRequest, Position};
use super::error::ApiError;
use super::tenants_api::TenantBody;

pub(crate) fn me_routes() -> Router<AppState> {
    Router::new().route("/me/workspaces", get(list_workspaces))
}

async fn list_workspaces(State(state): State<AppState>, caller: SignedIn, Query(query): Query<ListQuery>) -> Result<Response, ApiError> {
    if caller.session.kind != CredentialKind::BrowserSession {
        return Err(ApiError::forbidden());
    }
    let principal = caller.principal.to_string();
    let request = PageRequest::new(CursorScope::new("me-workspaces", &[&principal]), query.limit, query.cursor.as_deref())?;
    let after = request
        .after
        .as_ref()
        .map(|position| figcraft_cloud_domain::TenantId::try_from(position.id))
        .transpose()
        .map_err(|_| super::cursor::invalid_cursor())?;
    let size = PageSize::new(request.limit).map_err(|_| ApiError::new(figcraft_cloud_domain::ErrorCode::InvalidRequest))?;
    let page = tenants::list_tenants_for_principal(state.pool(), caller.principal, after, size).await.map_err(|_| ApiError::unavailable())?;
    let next_cursor = page.next_cursor.and_then(|id| request.cursor_for(&Position { sort_key: String::new(), id: id.as_uuid() }));
    let items = page.items.into_iter().map(|item| TenantBody::new(item.tenant, Some(item.role))).collect();
    Ok(Json(PageBody { items, next_cursor }).into_response())
}

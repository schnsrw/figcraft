//! Document history (S6-040): who changed a file, when and through what; the system's checkpoints;
//! "last edited by". Named versions and restore live in `versions_api` (S6-041).
//!
//! | Route (under `/api/v1/documents/{id}/history`) | What it returns |
//! |---|---|
//! | `GET activity?limit&cursor` | runs of consecutive batches by one person through one access, newest first: `{from_seq, to_seq, batches, started_at, ended_at, person{principal_id, display_name, color}, via{kind, id?, name?}\|null, label}`; `label` reads "Ada" or "Ada, through the Design team". A pause of more than five minutes, a different person or a different access starts a new run |
//! | `GET checkpoints?limit&cursor` | the verified snapshots the system keeps `{id, seq, created_at, size_bytes}`: an implementation detail, not something a person saved (see versions) |
//! | `GET summary` | `{current_seq, last_edit{seq, at, person, via, label}\|null}` for "last edited by" |
//!
//! Every route needs `document.view` on the document, exactly like reading the document: a person
//! who cannot see it gets `404`, a share-link session `403`. Pages are keyed by sequence number and
//! signed like every other cursor. The log's content is never returned here, only who and when.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use figcraft_cloud_auth::authz::Action;
use figcraft_cloud_domain::{CloudDocumentId, ErrorCode, Uuid};
use figcraft_cloud_storage::attribution::Via;
use figcraft_cloud_storage::history::{self, ActivityRun, HistoryError, LastEdit};
use serde_json::{Value, json};
use std::collections::BTreeMap;

use super::AppState;
use super::auth::Authenticated;
use super::cursor::{CursorScope, ListQuery, PageRequest, Position};
use super::documents_api::{begin, document_access};
use super::error::ApiError;

pub(crate) fn history_routes() -> Router<AppState> {
    Router::new()
        .route("/documents/{id}/history/activity", get(activity))
        .route("/documents/{id}/history/checkpoints", get(checkpoints))
        .route("/documents/{id}/history/summary", get(summary))
}

fn history_error(error: HistoryError) -> ApiError {
    match error {
        HistoryError::Database => ApiError::unavailable(),
        HistoryError::CorruptRow => ApiError::new(ErrorCode::Internal),
    }
}

fn via_body(via: Option<Via>, names: &BTreeMap<Uuid, String>) -> Value {
    match via {
        None => Value::Null,
        Some(via) => {
            let mut body = json!({ "kind": via.kind() });
            if let Some(id) = via.id() {
                body["id"] = json!(id.to_string());
                if let Some(name) = names.get(&id) {
                    body["name"] = json!(name);
                }
            }
            body
        }
    }
}

fn label(display_name: &str, via: Option<Via>, names: &BTreeMap<Uuid, String>) -> String {
    history::describe(display_name, via, via.and_then(Via::id).and_then(|id| names.get(&id)).map(String::as_str))
}

fn run_body(run: &ActivityRun, names: &BTreeMap<Uuid, String>) -> Value {
    json!({
        "from_seq": run.from_seq,
        "to_seq": run.to_seq,
        "batches": run.batches,
        "started_at": run.started_at,
        "ended_at": run.ended_at,
        "person": { "principal_id": run.principal_id.to_string(), "display_name": run.display_name, "color": run.color },
        "via": via_body(run.via, names),
        "label": label(&run.display_name, run.via, names),
    })
}

/// The sequence a cursor continues below (`None`: from the newest).
fn before_seq(request: &PageRequest) -> Result<Option<i64>, ApiError> {
    match &request.after {
        None => Ok(None),
        Some(position) => position.sort_key.parse::<i64>().map(Some).map_err(|_| super::cursor::invalid_cursor()),
    }
}

async fn activity(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(id): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<Response, ApiError> {
    let mut transaction = begin(&state, &caller).await?;
    let (document, _) = document_access(&mut transaction, &caller, &id, Action::DocumentView).await?;
    let tenant = transaction.tenant_id().to_string();
    let scope = CursorScope::new("document_activity", &[&tenant, &document.to_string()]);
    let request = PageRequest::new(scope, query.limit, query.cursor.as_deref())?;
    let page = history::read_activity(&mut transaction, document, before_seq(&request)?, request.limit as usize).await.map_err(history_error)?;
    let vias: Vec<Via> = page.runs.iter().filter_map(|run| run.via).collect();
    let names = history::via_names(&mut transaction, &vias).await.map_err(history_error)?;
    let _ = transaction.rollback().await;
    let next_cursor = page.next_before_seq.and_then(|seq| request.cursor_for(&Position { sort_key: seq.to_string(), id: document.as_uuid() }));
    let items: Vec<Value> = page.runs.iter().map(|run| run_body(run, &names)).collect();
    Ok(axum::Json(json!({ "items": items, "next_cursor": next_cursor })).into_response())
}

async fn checkpoints(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(id): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<Response, ApiError> {
    let mut transaction = begin(&state, &caller).await?;
    let (document, _) = document_access(&mut transaction, &caller, &id, Action::DocumentView).await?;
    let tenant = transaction.tenant_id().to_string();
    let scope = CursorScope::new("document_checkpoints", &[&tenant, &document.to_string()]);
    let request = PageRequest::new(scope, query.limit, query.cursor.as_deref())?;
    let rows = history::list_checkpoints(&mut transaction, document, before_seq(&request)?, i64::from(request.limit)).await.map_err(history_error)?;
    let _ = transaction.rollback().await;
    let (items, next_cursor) = request.finish(rows, |row| Position { sort_key: row.seq.to_string(), id: row.id });
    let items: Vec<Value> =
        items.iter().map(|c| json!({ "id": c.id.to_string(), "seq": c.seq, "created_at": c.created_at, "size_bytes": c.size_bytes })).collect();
    Ok(axum::Json(json!({ "items": items, "next_cursor": next_cursor })).into_response())
}

fn last_edit_body(edit: &LastEdit, names: &BTreeMap<Uuid, String>) -> Value {
    json!({
        "seq": edit.seq,
        "at": edit.at,
        "person": { "principal_id": edit.principal_id.to_string(), "display_name": edit.display_name, "color": edit.color },
        "via": via_body(edit.via, names),
        "label": label(&edit.display_name, edit.via, names),
    })
}

/// The "last edited by" summary of `document` in `transaction`'s tenant.
pub(crate) async fn summary_of(
    transaction: &mut figcraft_cloud_storage::TenantTransaction<'_>,
    document: CloudDocumentId,
) -> Result<Value, ApiError> {
    let last = history::last_edit(transaction, document).await.map_err(history_error)?;
    let names = match last.as_ref().and_then(|edit| edit.via) {
        Some(via) => history::via_names(transaction, &[via]).await.map_err(history_error)?,
        None => BTreeMap::new(),
    };
    let current: Option<i64> = sqlx::query_scalar("SELECT current_seq FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(transaction.tenant_id().to_string())
        .bind(document.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(|_| ApiError::unavailable())?;
    Ok(json!({ "current_seq": current.unwrap_or(0), "last_edit": last.as_ref().map(|edit| last_edit_body(edit, &names)) }))
}

async fn summary(State(state): State<AppState>, caller: Authenticated, Path(id): Path<String>) -> Result<Response, ApiError> {
    let mut transaction = begin(&state, &caller).await?;
    let (document, _) = document_access(&mut transaction, &caller, &id, Action::DocumentView).await?;
    let body = summary_of(&mut transaction, document).await?;
    let _ = transaction.rollback().await;
    Ok(axum::Json(body).into_response())
}

//! Named versions and restore (S6-041).
//!
//! | Route (under `/api/v1/documents/{id}/history/versions`) | Permission | What it does |
//! |---|---|---|
//! | `GET ?limit&cursor` | `document.view` | the document's saved versions, newest first: `{id, name, seq, created_at, created_by{principal_id, display_name}}` |
//! | `POST {name, seq?}` | `document.edit` | save the document as it is now (or as it was at `seq`) under a name. The state is rebuilt from the durable history first, so a listed version is always restorable; `409` when it cannot be rebuilt |
//! | `PATCH {id}` `{name}` | `document.edit` | rename |
//! | `DELETE {id}` | `document.edit` | hide the name (history is untouched) |
//! | `POST {id}/restore` | `document.edit` | make the document hold that version again |
//!
//! **Restore never rewrites history.** The state at the version's sequence is rebuilt, the changes
//! that turn the room's *current* state into it are computed by the collaboration client, and they are
//! submitted to the room through the relay as **one ordinary batch** under the caller's own realtime
//! ticket: every connected client sees a normal edit, the batch is attributed to the person who asked
//! (through the access they hold) and gets the next sequence number, and everything before it stays in
//! the log. `200 {restored: true, seq}` when a batch was added, `{restored: false, seq}` when the
//! document already was that version. `403` when the relay refuses the caller (no edit right), `503`
//! when the relay cannot be reached.
//!
//! Limits: at most 500 live versions per document; a restore replays at most the snapshot interval of
//! events beyond the nearest verified snapshot; the diff costs the size of the document, and a
//! document or state over the relay's message size cannot be restored (the relay refuses the batch).

use std::net::ToSocketAddrs;
use std::time::Duration;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use figcraft_cloud_auth::authz::Action;
use figcraft_cloud_domain::{CloudDocumentId, ErrorCode, Uuid};
use figcraft_cloud_relay::restore::{Applied, ApplyError, apply_document};
use figcraft_cloud_relay::server_at;
use figcraft_cloud_storage::versions::{self, Version, VersionError};
use figcraft_collab::Limits;
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use super::auth::Authenticated;
use super::cursor::{CursorScope, ListQuery, PageRequest, Position};
use super::documents_api::{audit, begin, commit, document_access};
use super::error::{ApiError, ApiJson};

/// How long rebuilding a state may take.
const REBUILD_TIMEOUT: Duration = Duration::from_secs(60);
/// How long one restore may take end to end.
const RESTORE_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) fn version_routes() -> Router<AppState> {
    Router::new()
        .route("/documents/{id}/history/versions", get(list).post(create))
        .route("/documents/{id}/history/versions/{version}", axum::routing::patch(rename).delete(remove))
        .route("/documents/{id}/history/versions/{version}/restore", post(restore))
}

fn version_error(error: VersionError) -> ApiError {
    match error {
        VersionError::InvalidName | VersionError::InvalidSequence => ApiError::new(ErrorCode::InvalidRequest),
        VersionError::TooMany => ApiError::new(ErrorCode::QuotaExceeded),
        VersionError::NotFound => ApiError::not_found(),
        VersionError::Database => ApiError::unavailable(),
        VersionError::Audit | VersionError::CorruptRow => ApiError::new(ErrorCode::Internal),
    }
}

fn body(version: &Version) -> Value {
    json!({
        "id": version.id.to_string(),
        "name": version.name,
        "seq": version.seq,
        "created_at": version.created_at,
        "created_by": { "principal_id": version.created_by.to_string(), "display_name": version.created_by_name },
    })
}

fn version_id(text: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(text).map_err(|_| ApiError::not_found())
}

async fn list(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(id): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<Response, ApiError> {
    let mut transaction = begin(&state, &caller).await?;
    let (document, _) = document_access(&mut transaction, &caller, &id, Action::DocumentView).await?;
    let tenant = transaction.tenant_id().to_string();
    let request = PageRequest::new(CursorScope::new("document_versions", &[&tenant, &document.to_string()]), query.limit, query.cursor.as_deref())?;
    let after = match &request.after {
        None => None,
        Some(position) => Some((position.sort_key.parse::<i64>().map_err(|_| super::cursor::invalid_cursor())?, position.id)),
    };
    let rows = versions::list_versions(&mut transaction, document, after, i64::from(request.limit)).await.map_err(version_error)?;
    let _ = transaction.rollback().await;
    let (rows, next_cursor) = request.finish(rows, |version| Position { sort_key: version.created_at.to_string(), id: version.id });
    Ok(axum::Json(json!({ "items": rows.iter().map(body).collect::<Vec<_>>(), "next_cursor": next_cursor })).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    name: String,
    seq: Option<i64>,
}

/// Prove the document can be rebuilt at `seq` (the state is thrown away).
async fn prove_restorable(state: &AppState, caller: &Authenticated, document: CloudDocumentId, seq: i64) -> Result<(), ApiError> {
    let rebuilt =
        tokio::time::timeout(REBUILD_TIMEOUT, server_at(state.pool(), state.objects(), caller.tenant.tenant_id(), document, seq, Limits::default()))
            .await;
    match rebuilt {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(_)) => Err(ApiError::new(ErrorCode::Conflict)),
        Err(_) => Err(ApiError::unavailable()),
    }
}

async fn create(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<CreateBody>,
) -> Result<Response, ApiError> {
    if versions::normalize_name(&request.name).is_none() || request.seq.is_some_and(|seq| seq < 0) {
        return Err(ApiError::new(ErrorCode::InvalidRequest));
    }
    let mut transaction = begin(&state, &caller).await?;
    let (document, _) = document_access(&mut transaction, &caller, &id, Action::DocumentEdit).await?;
    let current: Option<i64> = sqlx::query_scalar("SELECT current_seq FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(transaction.tenant_id().to_string())
        .bind(document.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(|_| ApiError::unavailable())?;
    let _ = transaction.rollback().await;
    let current = current.ok_or(ApiError::not_found())?;
    let seq = request.seq.unwrap_or(current);
    if seq > current {
        return Err(ApiError::new(ErrorCode::InvalidRequest));
    }
    prove_restorable(&state, &caller, document, seq).await?;
    let mut transaction = begin(&state, &caller).await?;
    let (document, _) = document_access(&mut transaction, &caller, &id, Action::DocumentEdit).await?;
    let version = versions::create_version(&mut transaction, document, seq, &request.name, caller.principal).await.map_err(version_error)?;
    commit(transaction).await?;
    Ok((StatusCode::CREATED, axum::Json(body(&version))).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameBody {
    name: String,
}

async fn rename(
    State(state): State<AppState>,
    caller: Authenticated,
    Path((id, version)): Path<(String, String)>,
    ApiJson(request): ApiJson<RenameBody>,
) -> Result<Response, ApiError> {
    let version = version_id(&version)?;
    let mut transaction = begin(&state, &caller).await?;
    let (document, _) = document_access(&mut transaction, &caller, &id, Action::DocumentEdit).await?;
    let renamed = versions::rename_version(&mut transaction, document, version, &request.name, caller.principal).await.map_err(version_error)?;
    commit(transaction).await?;
    Ok(axum::Json(body(&renamed)).into_response())
}

async fn remove(State(state): State<AppState>, caller: Authenticated, Path((id, version)): Path<(String, String)>) -> Result<Response, ApiError> {
    let version = version_id(&version)?;
    let mut transaction = begin(&state, &caller).await?;
    let (document, _) = document_access(&mut transaction, &caller, &id, Action::DocumentEdit).await?;
    versions::delete_version(&mut transaction, document, version, caller.principal).await.map_err(version_error)?;
    commit(transaction).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn restore(State(state): State<AppState>, caller: Authenticated, Path((id, version)): Path<(String, String)>) -> Result<Response, ApiError> {
    let version_id = version_id(&version)?;
    let mut transaction = begin(&state, &caller).await?;
    let (document, access) = document_access(&mut transaction, &caller, &id, Action::DocumentEdit).await?;
    let saved = versions::get_version(&mut transaction, document, version_id).await.map_err(version_error)?.ok_or(ApiError::not_found())?;
    let _ = transaction.rollback().await;
    let _ = access;
    // The state at the version, from the durable history alone.
    let rebuilt = tokio::time::timeout(
        REBUILD_TIMEOUT,
        server_at(state.pool(), state.objects(), caller.tenant.tenant_id(), document, saved.seq, Limits::default()),
    )
    .await
    .map_err(|_| ApiError::unavailable())?
    .map_err(|_| ApiError::new(ErrorCode::Conflict))?;
    let target = rebuilt.document().clone();
    drop(rebuilt);
    // One ordinary batch through the relay, under the caller's own ticket.
    let minted = super::realtime::mint_for(&state, &caller, document, true)?;
    let address = state.config().relay_addr.to_socket_addrs().ok().and_then(|mut addresses| addresses.next()).ok_or_else(ApiError::unavailable)?;
    let file = document.to_string();
    let applied = tokio::task::spawn_blocking(move || apply_document(address, &file, &minted.ticket, &target, RESTORE_TIMEOUT))
        .await
        .map_err(|_| ApiError::unavailable())?
        .map_err(|error| match error {
            ApplyError::Refused => ApiError::forbidden(),
            ApplyError::Rejected => ApiError::new(ErrorCode::Conflict),
            ApplyError::Connect | ApplyError::Timeout | ApplyError::Protocol => ApiError::unavailable(),
        })?;
    let (restored, seq) = match applied {
        Applied::Batch { seq } => (true, i64::try_from(seq).unwrap_or(i64::MAX)),
        Applied::Unchanged => (false, -1),
    };
    // Audit what was done (the restore itself is in the log as a batch by this person).
    let mut transaction = begin(&state, &caller).await?;
    let (document, _) = document_access(&mut transaction, &caller, &id, Action::DocumentView).await?;
    let head: Option<i64> = sqlx::query_scalar("SELECT current_seq FROM documents WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(transaction.tenant_id().to_string())
        .bind(document.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(|_| ApiError::unavailable())?;
    audit(
        &mut transaction,
        &caller,
        "document.version_restore",
        "document",
        document.as_uuid(),
        json!({ "version_id": saved.id.to_string(), "seq": saved.seq, "restored": restored }),
    )
    .await?;
    commit(transaction).await?;
    let seq = if seq >= 0 { seq } else { head.unwrap_or(0) };
    Ok(axum::Json(json!({ "restored": restored, "seq": seq, "version": body(&saved) })).into_response())
}

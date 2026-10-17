//! Platform admin management: list, create, disable, enable, reset the second factor.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use figcraft_cloud_domain::Uuid;
use figcraft_cloud_storage::platform::{PlatformAudit, PlatformOutcome, PlatformTransaction, begin_platform_transaction, record_platform_audit};
use figcraft_cloud_storage::platform_admins::{self, AdminState, PlatformAdmin};
use serde::Deserialize;

use super::{PlatformCaller, api_error, passwords, service_error};
use crate::api::AppState;
use crate::api::context::RequestContext;
use crate::api::error::{ApiError, ApiJson};

/// The wire form of an admin: never a hash or a secret.
pub(crate) fn admin_json(admin: &PlatformAdmin) -> serde_json::Value {
    serde_json::json!({
        "id": admin.id.to_string(),
        "login": admin.login,
        "display_name": admin.display_name,
        "state": admin.state.as_str(),
        "has_password": admin.has_password,
        "totp_enrolled": admin.totp_confirmed,
        "created_at": admin.created_at,
        "last_login_at": admin.last_login_at,
    })
}

async fn record(
    tx: &mut PlatformTransaction<'_>,
    context: &RequestContext,
    caller: &PlatformCaller,
    action: &str,
    target: Option<Uuid>,
) -> Result<(), ApiError> {
    let request_id = context.request_id.to_string();
    record_platform_audit(
        tx,
        &PlatformAudit {
            actor: caller.actor,
            action,
            resource_kind: "platform_admin",
            resource_id: target,
            tenant_id: None,
            outcome: PlatformOutcome::Succeeded,
            request_id: Some(&request_id),
            metadata: &serde_json::json!({}),
        },
    )
    .await
    .map_err(api_error)?;
    Ok(())
}

/// `GET /platform/v1/admins`
pub(crate) async fn list(State(state): State<AppState>, _caller: PlatformCaller) -> Result<Response, ApiError> {
    let mut tx = begin_platform_transaction(state.pool()).await.map_err(api_error)?;
    let admins = platform_admins::list_admins(&mut tx).await.map_err(api_error)?;
    let _ = tx.rollback().await;
    Ok(Json(serde_json::json!({ "items": admins.iter().map(admin_json).collect::<Vec<_>>() })).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateAdmin {
    login: String,
    display_name: String,
    password: String,
}

/// `POST /platform/v1/admins`: the new admin must enrol a second factor at first sign-in.
pub(crate) async fn create(
    State(state): State<AppState>,
    context: RequestContext,
    caller: PlatformCaller,
    ApiJson(body): ApiJson<CreateAdmin>,
) -> Result<Response, ApiError> {
    let login = platform_admins::normalize_login(&body.login).ok_or(ApiError::new(figcraft_cloud_domain::ErrorCode::InvalidRequest))?;
    let hash = passwords(&state)?.hash(body.password.clone(), Some(login)).await.map_err(service_error)?;
    let mut tx = begin_platform_transaction(state.pool()).await.map_err(api_error)?;
    let created = platform_admins::create_admin(&mut tx, Some(caller.admin_id), &body.login, &body.display_name, &hash).await.map_err(api_error)?;
    record(&mut tx, &context, &caller, "platform.admin.create", Some(created.id)).await?;
    tx.commit().await.map_err(api_error)?;
    Ok((StatusCode::CREATED, Json(admin_json(&created))).into_response())
}

async fn change_state(state: &AppState, context: &RequestContext, caller: &PlatformCaller, id: Uuid, to: AdminState) -> Result<Response, ApiError> {
    if id == caller.admin_id && to == AdminState::Disabled {
        // Disabling yourself would end your own session mid-request; ask another admin.
        return Err(ApiError::forbidden());
    }
    let mut tx = begin_platform_transaction(state.pool()).await.map_err(api_error)?;
    let changed = platform_admins::set_admin_state(&mut tx, id, to).await.map_err(api_error)?;
    let action = if to == AdminState::Disabled { "platform.admin.disable" } else { "platform.admin.enable" };
    record(&mut tx, context, caller, action, Some(id)).await?;
    tx.commit().await.map_err(api_error)?;
    Ok(Json(admin_json(&changed)).into_response())
}

/// `POST /platform/v1/admins/{id}/disable`: ends the admin's sessions; never the last active admin.
pub(crate) async fn disable(
    State(state): State<AppState>,
    context: RequestContext,
    caller: PlatformCaller,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    change_state(&state, &context, &caller, id, AdminState::Disabled).await
}

/// `POST /platform/v1/admins/{id}/enable`
pub(crate) async fn enable(
    State(state): State<AppState>,
    context: RequestContext,
    caller: PlatformCaller,
    Path(id): Path<Uuid>,
) -> Result<Response, ApiError> {
    change_state(&state, &context, &caller, id, AdminState::Active).await
}

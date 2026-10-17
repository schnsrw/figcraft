//! Bootstrap, sign-in (password then TOTP) and sign-out for platform admins.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use figcraft_cloud_domain::Uuid;
use figcraft_cloud_storage::platform::{PlatformActor, PlatformAudit, PlatformOutcome, begin_platform_transaction, record_platform_audit};
use figcraft_cloud_storage::platform_admins::{self, AdminCredentials, AdminState};
use serde::Deserialize;

use super::{
    LOCK_AFTER_FAILURES, LOCK_SECS, MAX_SIGN_IN_PASSWORD_BYTES, PartialSession, api_error, audit, clear_session_cookie, passwords, service_error,
    session_cookie, totp, with_cookie,
};
use crate::api::AppState;
use crate::api::authenticator::now_unix;
use crate::api::context::RequestContext;
use crate::api::error::{ApiError, ApiJson};

/// Issuer name shown by authenticator apps.
const TOTP_ISSUER: &str = "FigCraft Platform";

fn totp_aad(admin: Uuid) -> Vec<u8> {
    format!("platform-totp|{admin}").into_bytes()
}

fn next_step(confirmed: bool) -> &'static str {
    if confirmed { "totp" } else { "totp_enrol" }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BootstrapRequest {
    token: String,
    login: String,
    display_name: String,
    password: String,
}

/// `POST /platform/v1/bootstrap`: spend the one-time token to create the first platform admin.
pub(crate) async fn bootstrap(
    State(state): State<AppState>,
    context: RequestContext,
    ApiJson(body): ApiJson<BootstrapRequest>,
) -> Result<Response, ApiError> {
    let login = platform_admins::normalize_login(&body.login).ok_or(ApiError::new(figcraft_cloud_domain::ErrorCode::InvalidRequest))?;
    let hash = passwords(&state)?.hash(body.password.clone(), Some(login)).await.map_err(service_error)?;
    let mut tx = begin_platform_transaction(state.pool()).await.map_err(api_error)?;
    let created = match platform_admins::bootstrap_first_admin(&mut tx, &body.token, &body.login, &body.display_name, &hash).await {
        Ok(created) => created,
        Err(error) => {
            let _ = tx.rollback().await;
            audit(
                &state,
                &context,
                PlatformActor::Anonymous,
                "platform.bootstrap",
                "platform_admin",
                None,
                None,
                PlatformOutcome::Denied,
                &serde_json::json!({}),
            )
            .await;
            return Err(api_error(error));
        }
    };
    let request_id = context.request_id.to_string();
    record_platform_audit(
        &mut tx,
        &PlatformAudit {
            actor: PlatformActor::Admin(created.id),
            action: "platform.bootstrap",
            resource_kind: "platform_admin",
            resource_id: Some(created.id),
            tenant_id: None,
            outcome: PlatformOutcome::Succeeded,
            request_id: Some(&request_id),
            metadata: &serde_json::json!({}),
        },
    )
    .await
    .map_err(api_error)?;
    tx.commit().await.map_err(api_error)?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "admin": super::admins::admin_json(&created) }))).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SignInRequest {
    login: String,
    password: String,
}

/// `POST /platform/v1/session`: check the password and open a session that still needs the second
/// factor. Every refusal is the same `401`; an unknown login costs the same as a wrong password.
pub(crate) async fn sign_in(
    State(state): State<AppState>,
    context: RequestContext,
    ApiJson(body): ApiJson<SignInRequest>,
) -> Result<Response, ApiError> {
    let service = passwords(&state)?;
    let mut tx = begin_platform_transaction(state.pool()).await.map_err(api_error)?;
    let credentials = platform_admins::credentials_by_login(&mut tx, &body.login).await.map_err(api_error)?;
    let stored = credentials.as_ref().and_then(|c| c.password_hash.clone());
    let password = body.password;
    // An unknown login, an admin without a password and an oversized password all cost one hash.
    let verdict = match stored {
        Some(stored) if password.len() <= MAX_SIGN_IN_PASSWORD_BYTES => service.verify(stored, password).await,
        _ => service.verify_unknown(password.chars().take(MAX_SIGN_IN_PASSWORD_BYTES).collect()).await,
    }
    .map_err(service_error)?;
    let matched = matches!(verdict, figcraft_cloud_auth::password::Verification::Valid { .. });
    let usable: Option<&AdminCredentials> = credentials.as_ref().filter(|c| c.admin.state == AdminState::Active && c.locked_for_secs == 0);
    let Some(credentials) = usable.filter(|_| matched) else {
        if let Some(known) = credentials.as_ref().filter(|c| c.admin.state == AdminState::Active) {
            let _ = platform_admins::record_failed_login(&mut tx, known.admin.id, LOCK_AFTER_FAILURES, LOCK_SECS).await;
        }
        let actor = credentials.as_ref().map_or(PlatformActor::Anonymous, |c| PlatformActor::Admin(c.admin.id));
        tx.commit().await.map_err(api_error)?;
        audit(&state, &context, actor, "platform.login.password", "platform_session", None, None, PlatformOutcome::Denied, &serde_json::json!({}))
            .await;
        return Err(ApiError::unauthenticated());
    };
    let session = platform_admins::create_session(&mut tx, credentials.admin.id, false, state.platform().limits).await.map_err(api_error)?;
    let request_id = context.request_id.to_string();
    record_platform_audit(
        &mut tx,
        &PlatformAudit {
            actor: PlatformActor::Admin(credentials.admin.id),
            action: "platform.login.password",
            resource_kind: "platform_session",
            resource_id: Some(session.id),
            tenant_id: None,
            outcome: PlatformOutcome::Succeeded,
            request_id: Some(&request_id),
            metadata: &serde_json::json!({}),
        },
    )
    .await
    .map_err(api_error)?;
    tx.commit().await.map_err(api_error)?;
    let response = Json(serde_json::json!({
        "admin": super::admins::admin_json(&credentials.admin),
        "second_factor_done": false,
        "next": next_step(credentials.admin.totp_confirmed),
    }))
    .into_response();
    Ok(with_cookie(response, &session_cookie(&session.token, session.absolute_remaining_secs)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TotpRequest {
    code: String,
}

/// `POST /platform/v1/session/totp/enrol`: create (or replace, before it is confirmed) the TOTP
/// secret. Returned once, for the authenticator app; it counts after the first verified code.
pub(crate) async fn enrol_totp(State(state): State<AppState>, context: RequestContext, session: PartialSession) -> Result<Response, ApiError> {
    if session.second_factor_done {
        return Err(ApiError::new(figcraft_cloud_domain::ErrorCode::Conflict));
    }
    let key = state.identity().and_then(|identity| identity.secret_key()).ok_or(ApiError::unavailable())?;
    let mut tx = begin_platform_transaction(state.pool()).await.map_err(api_error)?;
    let credentials = platform_admins::credentials_by_login(&mut tx, &session.login).await.map_err(api_error)?.ok_or(ApiError::unauthenticated())?;
    if credentials.admin.totp_confirmed {
        let _ = tx.rollback().await;
        return Err(ApiError::new(figcraft_cloud_domain::ErrorCode::Conflict));
    }
    let secret = totp::generate_secret().ok_or(ApiError::new(figcraft_cloud_domain::ErrorCode::Internal))?;
    let sealed = key.seal(&totp_aad(session.admin_id), &secret).map_err(|_| ApiError::new(figcraft_cloud_domain::ErrorCode::Internal))?;
    platform_admins::store_totp_secret(&mut tx, session.admin_id, &sealed).await.map_err(api_error)?;
    tx.commit().await.map_err(api_error)?;
    audit(
        &state,
        &context,
        PlatformActor::Admin(session.admin_id),
        "platform.totp.enrol",
        "platform_admin",
        Some(session.admin_id),
        None,
        PlatformOutcome::Succeeded,
        &serde_json::json!({}),
    )
    .await;
    let uri = totp::provisioning_uri(TOTP_ISSUER, &session.login, &secret);
    Ok(([(axum::http::header::CACHE_CONTROL, "no-store")], Json(serde_json::json!({ "secret": totp::base32(&secret), "otpauth_uri": uri })))
        .into_response())
}

/// `POST /platform/v1/session/totp`: verify a code. The first verified code also confirms the
/// enrolment. A step is accepted once; failures count toward the lock.
pub(crate) async fn verify_totp(
    State(state): State<AppState>,
    context: RequestContext,
    session: PartialSession,
    ApiJson(body): ApiJson<TotpRequest>,
) -> Result<Response, ApiError> {
    let key = state.identity().and_then(|identity| identity.secret_key()).ok_or(ApiError::unavailable())?;
    let mut tx = begin_platform_transaction(state.pool()).await.map_err(api_error)?;
    let credentials = platform_admins::credentials_by_login(&mut tx, &session.login).await.map_err(api_error)?.ok_or(ApiError::unauthenticated())?;
    if credentials.locked_for_secs > 0 {
        let _ = tx.rollback().await;
        return Err(ApiError::unauthenticated());
    }
    let sealed = credentials.totp_secret.as_deref().ok_or(ApiError::new(figcraft_cloud_domain::ErrorCode::Conflict))?;
    let secret = key.open(&totp_aad(session.admin_id), sealed).map_err(|_| ApiError::new(figcraft_cloud_domain::ErrorCode::Internal))?;
    let step = totp::verify(&secret, body.code.trim(), now_unix());
    let accepted = match step {
        Some(step) => platform_admins::accept_totp_step(&mut tx, session.admin_id, step).await.map_err(api_error)?,
        None => false,
    };
    if !accepted {
        let _ = platform_admins::record_failed_login(&mut tx, session.admin_id, LOCK_AFTER_FAILURES, LOCK_SECS).await;
        tx.commit().await.map_err(api_error)?;
        audit(
            &state,
            &context,
            PlatformActor::Admin(session.admin_id),
            "platform.login.totp",
            "platform_session",
            Some(session.session_id),
            None,
            PlatformOutcome::Denied,
            &serde_json::json!({}),
        )
        .await;
        return Err(ApiError::unauthenticated());
    }
    if !credentials.admin.totp_confirmed {
        platform_admins::confirm_totp(&mut tx, session.admin_id).await.map_err(api_error)?;
    }
    platform_admins::mark_second_factor_done(&mut tx, session.session_id).await.map_err(api_error)?;
    platform_admins::record_successful_login(&mut tx, session.admin_id).await.map_err(api_error)?;
    let request_id = context.request_id.to_string();
    record_platform_audit(
        &mut tx,
        &PlatformAudit {
            actor: PlatformActor::Admin(session.admin_id),
            action: "platform.login.totp",
            resource_kind: "platform_session",
            resource_id: Some(session.session_id),
            tenant_id: None,
            outcome: PlatformOutcome::Succeeded,
            request_id: Some(&request_id),
            metadata: &serde_json::json!({ "enrolled_now": !credentials.admin.totp_confirmed }),
        },
    )
    .await
    .map_err(api_error)?;
    tx.commit().await.map_err(api_error)?;
    Ok(Json(serde_json::json!({ "second_factor_done": true })).into_response())
}

/// `GET /platform/v1/session`: who am I and which step is next.
pub(crate) async fn current(State(state): State<AppState>, session: PartialSession) -> Result<Response, ApiError> {
    let mut tx = begin_platform_transaction(state.pool()).await.map_err(api_error)?;
    let admin = platform_admins::get_admin(&mut tx, session.admin_id).await.map_err(api_error)?.ok_or(ApiError::unauthenticated())?;
    let _ = tx.rollback().await;
    let next = if session.second_factor_done { "none" } else { next_step(admin.totp_confirmed) };
    Ok(Json(serde_json::json!({ "admin": super::admins::admin_json(&admin), "second_factor_done": session.second_factor_done, "next": next }))
        .into_response())
}

/// `DELETE /platform/v1/session`: sign out.
pub(crate) async fn sign_out(State(state): State<AppState>, context: RequestContext, session: PartialSession) -> Result<Response, ApiError> {
    let mut tx = begin_platform_transaction(state.pool()).await.map_err(api_error)?;
    platform_admins::revoke_session(&mut tx, session.session_id).await.map_err(api_error)?;
    tx.commit().await.map_err(api_error)?;
    audit(
        &state,
        &context,
        PlatformActor::Admin(session.admin_id),
        "platform.logout",
        "platform_session",
        Some(session.session_id),
        None,
        PlatformOutcome::Succeeded,
        &serde_json::json!({}),
    )
    .await;
    Ok(with_cookie(StatusCode::NO_CONTENT.into_response(), &clear_session_cookie()))
}

//! Realtime tickets over HTTP (S4-019): `POST /api/v1/documents/{document_id}/realtime-ticket`.
//!
//! A signed-in browser asks for a ticket, then sends it as the token of its relay hello. The
//! ticket (`figcraft_cloud_storage::realtime_tickets`) is stateless and HMAC-signed with a subkey
//! of `SESSION_SIGNING_KEY`; it binds the tenant, document, principal, browser session and access,
//! and lives [`TICKET_SECS`] seconds. The relay verifies it with the same key and re-checks the
//! session and effective role on admission, then watches the member for revocation.
//!
//! Order of checks: [`Authenticated`] (`401`) → browser sessions only (service accounts and share
//! links `403`: a ticket names a browser session) → the ticket key (`503` without
//! `SESSION_SIGNING_KEY`) → tenant transaction → effective document role → disclosure policy (a
//! document the caller cannot see is `404`; visible without `document.join_realtime` is `403`).
//! Access is `edit` only when the caller holds `document.edit`.
//!
//! The answer is `{ticket, expires_in, ws_url, access, document_id}` with `Cache-Control:
//! no-store`. Nothing logs the ticket.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use figcraft_cloud_domain::{CloudDocumentId, Uuid};
use figcraft_cloud_storage::realtime_tickets::{MAX_TICKET_SECS, RealtimeTicket, TicketKey, expiry, mint};
use serde_json::json;

use super::AppState;
use super::auth::{Authenticated, CredentialKind};
use super::error::ApiError;

/// Lifetime of every ticket minted here (the storage cap).
pub(crate) const TICKET_SECS: u64 = MAX_TICKET_SECS;

/// The ticket key for `SESSION_SIGNING_KEY`, `None` without one or when it is too short (the
/// identity stack refuses such a key already).
pub(crate) fn ticket_key(signing_key: Option<&[u8]>) -> Option<Arc<TicketKey>> {
    signing_key.and_then(|secret| TicketKey::derive(secret).ok()).map(Arc::new)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_secs())
}

/// A minted ticket and what it grants.
pub(crate) struct Minted {
    pub(crate) ticket: String,
    pub(crate) edit: bool,
}

/// Mint a ticket for `caller` on `document`, whose access was already authorized (`edit` is the
/// caller's `document.edit` decision). Browser sessions only; `503` without a ticket key.
pub(crate) fn mint_for(state: &AppState, caller: &Authenticated, document: CloudDocumentId, edit: bool) -> Result<Minted, ApiError> {
    if caller.session.kind != CredentialKind::BrowserSession {
        return Err(ApiError::forbidden());
    }
    let key = state.realtime_key().ok_or_else(ApiError::unavailable)?;
    let session = Uuid::parse_str(&caller.session.id).map_err(|_| ApiError::forbidden())?;
    let ticket = RealtimeTicket {
        tenant: caller.tenant.tenant_id(),
        document,
        principal: caller.principal,
        session,
        edit,
        expires_at: expiry(now_secs(), TICKET_SECS),
    };
    Ok(Minted { ticket: mint(key, &ticket), edit })
}

/// The relay endpoint for `public_url` (`ws(s)://<host>/ws/v1/collab`).
pub(crate) fn ws_url(public_url: &str) -> Result<String, ApiError> {
    let base = public_url.trim_end_matches('/');
    let socket = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        return Err(ApiError::unavailable());
    };
    Ok(format!("{socket}{}", crate::relay::RELAY_PATH))
}

pub(crate) async fn realtime_ticket_handler(
    State(state): State<AppState>,
    caller: Authenticated,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    // Refuse credentials that can never hold a ticket before any lookup.
    if caller.session.kind != CredentialKind::BrowserSession {
        return Err(ApiError::forbidden());
    }
    // Authorize first so an unconfigured deployment answers the disclosure policy's 404/403 like
    // every other document route, and only an authorized caller learns tickets are unavailable.
    let (document, edit) = super::documents_api::authorize_realtime(&state, &caller, &id).await?;
    if state.realtime_key().is_none() {
        return Err(ApiError::unavailable());
    }
    let minted = mint_for(&state, &caller, document, edit)?;
    let mut response = Json(json!({
        "document_id": document.to_string(),
        "ticket": minted.ticket,
        "expires_in": TICKET_SECS,
        "ws_url": ws_url(super::documents_api::public_url())?,
        "access": if minted.edit { "edit" } else { "view" },
    }))
    .into_response();
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_urls_follow_the_public_scheme() {
        assert_eq!(ws_url("https://figcraft.example/").ok().as_deref(), Some("wss://figcraft.example/ws/v1/collab"));
        assert_eq!(ws_url("http://127.0.0.1:8080").ok().as_deref(), Some("ws://127.0.0.1:8080/ws/v1/collab"));
        assert!(ws_url("ftp://x").is_err());
    }

    #[test]
    fn ticket_keys_need_a_real_signing_key() {
        assert!(ticket_key(None).is_none());
        assert!(ticket_key(Some(b"short")).is_none());
        assert!(ticket_key(Some(b"e1-a-long-enough-signing-key")).is_some());
    }
}

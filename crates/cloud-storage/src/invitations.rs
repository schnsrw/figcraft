//! Team invitations (S4-009): invite an email address into a team with a role, accept, revoke and
//! list pending invitations.
//!
//! An invitation token is 32 random bytes from the operating system's generator, written as
//! URL-safe base64 without padding (43 characters). It is returned once by
//! [`create_invitation`]; only its SHA-256 is stored and the lookup is by that hash (unique
//! index), so the token is never compared in memory or logged. Every refusal that a token holder
//! could probe (unknown, malformed, expired, revoked or already accepted) is the same
//! [`InvitationError::Refused`].
//!
//! Acceptance names its tenant through the token, so [`accept_invitation`] starts from the pool,
//! then does everything in one tenant transaction: it locks the invitation row (`FOR UPDATE`, with
//! the pending predicate re-evaluated after any wait, so of two concurrent accepts exactly one
//! wins), checks the tenant and team are active, adds or reactivates the tenant membership (role
//! `viewer` for new members) and the team membership, marks the invitation accepted and records
//! `invitation.accept`, then commits.

use base64::Engine as _;
use figcraft_cloud_domain::{PrincipalId, TenantId, Uuid};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::fmt;

use crate::metadata::{Page, PageSize, page};
use crate::teams::{TeamError, TeamRole, upsert_member};
use crate::tenants::audit_success;
use crate::{TenantTransaction, begin_tenant_transaction};

/// Random bytes in an invitation token.
pub const TOKEN_BYTES: usize = 32;
/// Characters in an encoded token.
pub const TOKEN_CHARS: usize = 43;
/// Default lifetime: seven days.
pub const DEFAULT_LIFETIME_SECS: u64 = 7 * 24 * 60 * 60;
/// Longest lifetime an invitation may be given: thirty days.
pub const MAX_LIFETIME_SECS: u64 = 30 * 24 * 60 * 60;
/// Longest email address accepted (RFC 5321 path limit used by the schema).
pub const MAX_EMAIL_CHARS: usize = 320;

/// Fixed, non-leaking categories for invitation failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvitationError {
    InvalidEmail,
    InvalidLifetime,
    InvalidPageSize,
    Random,
    /// The token does not open a pending invitation (unknown, malformed, expired, revoked or
    /// already accepted). Deliberately one category.
    Refused,
    /// The tenant is suspended or otherwise not active.
    TenantInactive,
    /// The team does not exist in this tenant or is not active.
    TeamUnavailable,
    /// The accepting principal is not an active user, or their tenant membership is suspended.
    PrincipalUnavailable,
    /// The accepting principal's verified email is not the invited address.
    EmailMismatch,
    Audit,
    Database,
    CorruptRow,
}

impl fmt::Display for InvitationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invitation error: {self:?}")
    }
}

impl std::error::Error for InvitationError {}

fn database_error(error: &sqlx::Error) -> InvitationError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23503") => InvitationError::TeamUnavailable,
        _ => InvitationError::Database,
    }
}

fn team_error(error: TeamError) -> InvitationError {
    match error {
        TeamError::InvalidState => InvitationError::PrincipalUnavailable,
        TeamError::CorruptRow => InvitationError::CorruptRow,
        _ => InvitationError::Database,
    }
}

/// An invitation just created: its id, expiry and the token, which is never readable again.
#[derive(Clone, Eq, PartialEq)]
pub struct CreatedInvitation {
    pub id: Uuid,
    pub token: String,
}

impl fmt::Debug for CreatedInvitation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("CreatedInvitation").field("id", &self.id).field("token", &"[REDACTED]").finish()
    }
}

/// A pending invitation as listed to team managers (never includes the token).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvitationRecord {
    pub id: Uuid,
    pub team_id: Uuid,
    pub email: String,
    pub role: TeamRole,
    pub invited_by: PrincipalId,
    /// Seconds until the invitation expires (at least zero).
    pub expires_in_secs: i64,
}

/// What an accepted invitation granted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AcceptedInvitation {
    pub id: Uuid,
    pub tenant_id: TenantId,
    pub team_id: Uuid,
    pub team_role: TeamRole,
    /// Whether a tenant membership was created or reactivated (false when one was already active).
    pub joined_tenant: bool,
}

/// The SHA-256 stored for `token`.
#[must_use]
pub fn invitation_token_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn new_token() -> Result<String, InvitationError> {
    let mut bytes = [0u8; TOKEN_BYTES];
    rustls::crypto::ring::default_provider().secure_random.fill(&mut bytes).map_err(|_| InvitationError::Random)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

pub(crate) fn well_formed(token: &str) -> bool {
    token.len() == TOKEN_CHARS && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Normalise an email address for storage and comparison: trimmed and ASCII-lowercased, one `@`
/// with a non-empty local part and a dotted domain, no whitespace or control characters, at most
/// [`MAX_EMAIL_CHARS`]. This is a shape check only; ownership is proven by the identity provider.
#[must_use]
pub fn normalize_email(email: &str) -> Option<String> {
    let email = email.trim();
    if email.len() < 3 || email.chars().count() > MAX_EMAIL_CHARS || email.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let (local, domain) = email.split_once('@')?;
    if local.is_empty() || domain.contains('@') || !domain.contains('.') || domain.starts_with('.') || domain.ends_with('.') {
        return None;
    }
    Some(email.to_ascii_lowercase())
}

/// Invite `email` into active team `team_id` of the transaction's (active) tenant with `role`.
/// `expires_in_secs` defaults to seven days and is at most thirty. Records `invitation.create`
/// (the address itself is not copied into the audit trail). The caller authorizes and commits.
pub async fn create_invitation(
    transaction: &mut TenantTransaction<'_>,
    team_id: Uuid,
    email: &str,
    role: TeamRole,
    invited_by: PrincipalId,
    expires_in_secs: Option<u64>,
) -> Result<CreatedInvitation, InvitationError> {
    let email = normalize_email(email).ok_or(InvitationError::InvalidEmail)?;
    let lifetime = expires_in_secs.unwrap_or(DEFAULT_LIFETIME_SECS);
    if lifetime == 0 || lifetime > MAX_LIFETIME_SECS {
        return Err(InvitationError::InvalidLifetime);
    }
    let lifetime = i64::try_from(lifetime).map_err(|_| InvitationError::InvalidLifetime)?;
    let states: Option<(String, String)> = sqlx::query_as(
        "SELECT t.state, tm.state FROM tenants t JOIN teams tm ON tm.tenant_id = t.id \
         WHERE t.id = $1::uuid AND tm.id = $2::uuid FOR SHARE",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(team_id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    match states.as_ref().map(|(tenant, team)| (tenant.as_str(), team.as_str())) {
        None => return Err(InvitationError::TeamUnavailable),
        Some((tenant, _)) if tenant != "active" => return Err(InvitationError::TenantInactive),
        Some((_, team)) if team != "active" => return Err(InvitationError::TeamUnavailable),
        Some(_) => {}
    }
    let token = new_token()?;
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO team_invitations (id, tenant_id, team_id, email, role, token_hash, invited_by, expires_at) \
         VALUES ($1::uuid, $2::uuid, $3::uuid, $4, $5, $6, $7::uuid, now() + make_interval(secs => $8::bigint))",
    )
    .bind(id.to_string())
    .bind(transaction.tenant_id().to_string())
    .bind(team_id.to_string())
    .bind(&email)
    .bind(role.as_str())
    .bind(invitation_token_hash(&token).to_vec())
    .bind(invited_by.to_string())
    .bind(lifetime)
    .execute(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    let metadata = serde_json::json!({ "team_id": team_id.to_string(), "role": role.as_str(), "expires_in_secs": lifetime });
    audit_success(transaction, Some(invited_by), "invitation.create", "team_invitation", Some(id), &metadata)
        .await
        .map_err(|_| InvitationError::Audit)?;
    Ok(CreatedInvitation { id, token })
}

/// Revoke pending invitation `id`: true when it was pending and is now revoked, false when there
/// is no pending invitation with that id (unknown, accepted or already revoked). Expired but
/// unaccepted invitations can still be revoked. Records `invitation.revoke`.
pub async fn revoke_invitation(transaction: &mut TenantTransaction<'_>, id: Uuid, actor: PrincipalId) -> Result<bool, InvitationError> {
    let team: Option<String> = sqlx::query_scalar(
        "UPDATE team_invitations SET revoked_at = now() \
         WHERE tenant_id = $1::uuid AND id = $2::uuid AND revoked_at IS NULL AND accepted_at IS NULL RETURNING team_id::text",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    let Some(team) = team else {
        return Ok(false);
    };
    audit_success(transaction, Some(actor), "invitation.revoke", "team_invitation", Some(id), &serde_json::json!({ "team_id": team }))
        .await
        .map_err(|_| InvitationError::Audit)?;
    Ok(true)
}

/// Pending (unaccepted, unrevoked, unexpired) invitations in the transaction's tenant, optionally
/// for one team, ordered by id and keyset-paged.
pub async fn list_pending_invitations(
    transaction: &mut TenantTransaction<'_>,
    team_id: Option<Uuid>,
    after: Option<Uuid>,
    size: PageSize,
) -> Result<Page<InvitationRecord, Uuid>, InvitationError> {
    let rows: Vec<(String, String, String, String, String, i64)> = sqlx::query_as(
        "SELECT id::text, team_id::text, email, role, invited_by::text, \
                GREATEST(0, floor(extract(epoch FROM expires_at - now())))::bigint \
         FROM team_invitations WHERE tenant_id = $1::uuid AND ($2::uuid IS NULL OR team_id = $2::uuid) \
           AND accepted_at IS NULL AND revoked_at IS NULL AND expires_at > now() \
           AND ($3::uuid IS NULL OR id > $3::uuid) ORDER BY id LIMIT $4",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(team_id.map(|id| id.to_string()))
    .bind(after.map(|id| id.to_string()))
    .bind(size.query_limit())
    .fetch_all(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    let items = rows
        .into_iter()
        .map(|(id, team, email, role, invited_by, expires_in_secs)| {
            Ok(InvitationRecord {
                id: Uuid::parse_str(&id).map_err(|_| InvitationError::CorruptRow)?,
                team_id: Uuid::parse_str(&team).map_err(|_| InvitationError::CorruptRow)?,
                email,
                role: TeamRole::parse(&role).ok_or(InvitationError::CorruptRow)?,
                invited_by: invited_by.parse().map_err(|_| InvitationError::CorruptRow)?,
                expires_in_secs,
            })
        })
        .collect::<Result<Vec<_>, InvitationError>>()?;
    Ok(page(items, size, |item| item.id))
}

/// Accept the invitation `token` names, as `principal` (an active `user`). When the identity
/// layer has a verified email for the principal it passes it as `verified_email`, and it must
/// equal the invited address (case-insensitively); `None` skips that check (for example a
/// provider that issues no email claim). New tenant members join as `viewer`; an already active
/// member keeps their tenant role, and an already active team member keeps their team role.
/// Commits on success; on any error nothing changes.
pub async fn accept_invitation(
    pool: &PgPool,
    token: &str,
    principal: PrincipalId,
    verified_email: Option<&str>,
) -> Result<AcceptedInvitation, InvitationError> {
    if !well_formed(token) {
        return Err(InvitationError::Refused);
    }
    let hash = invitation_token_hash(token).to_vec();
    // The token names the tenant, so this one lookup runs through the SECURITY DEFINER function
    // (S4-014); everything else happens inside that tenant.
    let tenant: Option<String> = sqlx::query_scalar("SELECT tenant FROM figcraft_invitation_tenant($1) AS tenant WHERE tenant IS NOT NULL")
        .bind(&hash)
        .fetch_optional(pool)
        .await
        .map_err(|error| database_error(&error))?;
    let tenant_id: TenantId = tenant.ok_or(InvitationError::Refused)?.parse().map_err(|_| InvitationError::CorruptRow)?;
    let mut transaction = begin_tenant_transaction(pool, tenant_id).await.map_err(|_| InvitationError::Database)?;
    let accepted = accept_in(&mut transaction, &hash, principal, verified_email).await?;
    transaction.commit().await.map_err(|_| InvitationError::Database)?;
    Ok(accepted)
}

pub(crate) async fn accept_in(
    transaction: &mut TenantTransaction<'_>,
    hash: &[u8],
    principal: PrincipalId,
    verified_email: Option<&str>,
) -> Result<AcceptedInvitation, InvitationError> {
    let invitation: Option<(String, String, String, String)> = sqlx::query_as(
        "SELECT id::text, team_id::text, email, role FROM team_invitations \
         WHERE tenant_id = $1::uuid AND token_hash = $2 AND accepted_at IS NULL AND revoked_at IS NULL AND expires_at > now() \
         FOR UPDATE",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(hash)
    .fetch_optional(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    let (id, team, email, role) = invitation.ok_or(InvitationError::Refused)?;
    let id = Uuid::parse_str(&id).map_err(|_| InvitationError::CorruptRow)?;
    let team_id = Uuid::parse_str(&team).map_err(|_| InvitationError::CorruptRow)?;
    let team_role = TeamRole::parse(&role).ok_or(InvitationError::CorruptRow)?;

    let tenant_state: Option<String> = sqlx::query_scalar("SELECT state FROM tenants WHERE id = $1::uuid FOR SHARE")
        .bind(transaction.tenant_id().to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    if tenant_state.as_deref() != Some("active") {
        return Err(InvitationError::TenantInactive);
    }
    // Lock the team row as the team module does for every membership change.
    let team_state: Option<String> = sqlx::query_scalar("SELECT state FROM teams WHERE tenant_id = $1::uuid AND id = $2::uuid FOR UPDATE")
        .bind(transaction.tenant_id().to_string())
        .bind(team_id.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    if team_state.as_deref() != Some("active") {
        return Err(InvitationError::TeamUnavailable);
    }
    let principal_ok: Option<bool> = sqlx::query_scalar("SELECT kind = 'user' AND state = 'active' FROM principals WHERE id = $1::uuid FOR SHARE")
        .bind(principal.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    if principal_ok != Some(true) {
        return Err(InvitationError::PrincipalUnavailable);
    }
    if let Some(verified) = verified_email
        && normalize_email(verified).as_deref() != Some(email.as_str())
    {
        return Err(InvitationError::EmailMismatch);
    }

    let membership: Option<String> =
        sqlx::query_scalar("SELECT state FROM tenant_members WHERE tenant_id = $1::uuid AND principal_id = $2::uuid FOR UPDATE")
            .bind(transaction.tenant_id().to_string())
            .bind(principal.to_string())
            .fetch_optional(transaction.connection())
            .await
            .map_err(|error| database_error(&error))?;
    let joined_tenant = match membership.as_deref() {
        Some("active") => false,
        Some("suspended") => return Err(InvitationError::PrincipalUnavailable),
        Some(_) => {
            sqlx::query(
                "UPDATE tenant_members SET state = 'active', role = 'viewer', updated_at = now() \
                 WHERE tenant_id = $1::uuid AND principal_id = $2::uuid",
            )
            .bind(transaction.tenant_id().to_string())
            .bind(principal.to_string())
            .execute(transaction.connection())
            .await
            .map_err(|error| database_error(&error))?;
            true
        }
        None => {
            sqlx::query("INSERT INTO tenant_members (tenant_id, principal_id, role) VALUES ($1::uuid, $2::uuid, 'viewer')")
                .bind(transaction.tenant_id().to_string())
                .bind(principal.to_string())
                .execute(transaction.connection())
                .await
                .map_err(|error| database_error(&error))?;
            true
        }
    };
    let joined_team = upsert_member(transaction, team_id, principal, team_role).await.map_err(team_error)?;

    sqlx::query("UPDATE team_invitations SET accepted_at = now(), accepted_by = $3::uuid WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(transaction.tenant_id().to_string())
        .bind(id.to_string())
        .bind(principal.to_string())
        .execute(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    let metadata = serde_json::json!({ "team_id": team_id.to_string(), "role": team_role.as_str(),
        "joined_tenant": joined_tenant, "joined_team": joined_team });
    audit_success(transaction, Some(principal), "invitation.accept", "team_invitation", Some(id), &metadata)
        .await
        .map_err(|_| InvitationError::Audit)?;
    Ok(AcceptedInvitation { id, tenant_id: transaction.tenant_id(), team_id, team_role, joined_tenant })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_43_url_safe_characters_and_distinct() {
        let (a, b) = (new_token().unwrap(), new_token().unwrap());
        assert_eq!(a.len(), TOKEN_CHARS);
        assert!(well_formed(&a) && well_formed(&b));
        assert_ne!(a, b);
        assert!(!well_formed(&"a".repeat(42)));
        assert!(!well_formed(&format!("{}=", "a".repeat(42))));
    }

    #[test]
    fn emails_are_normalised_and_shape_checked() {
        assert_eq!(normalize_email("  Ada@Example.COM "), Some("ada@example.com".to_owned()));
        for bad in ["", "a@b", "@example.com", "ada@", "ada@@example.com", "a da@example.com", "ada@.com", "ada@example.", "a\u{0}@x.io"] {
            assert_eq!(normalize_email(bad), None, "{bad}");
        }
        assert_eq!(normalize_email(&format!("{}@example.com", "a".repeat(320))), None);
    }

    #[test]
    fn created_invitations_never_print_their_token() {
        let created = CreatedInvitation { id: Uuid::nil(), token: "top-secret".into() };
        assert!(!format!("{created:?}").contains("top-secret"));
    }
}

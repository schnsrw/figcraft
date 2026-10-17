//! Browser sessions (S4-002).
//!
//! A session token is 32 random bytes from the operating system's generator, written as URL-safe
//! base64 without padding (43 characters). [`create_session`] and [`rotate_session`] return it
//! once; only its SHA-256 is stored in `browser_sessions.token_hash` (unique index), so a lookup
//! never compares or logs the token itself.
//!
//! A session is live while it is not revoked, both its idle and absolute expiry lie in the future
//! and its principal is `active`. [`resolve_session`] slides the idle expiry (never past the
//! absolute expiry), writing at most once per [`SessionLimits::touch_interval_secs`] so a busy
//! client does not turn every request into a write. Every refusal (unknown, malformed, revoked,
//! idle- or absolute-expired, suspended principal) is the same [`SessionError::Refused`].
//!
//! Rotation ([`rotate_session`]) revokes the old row with reason `rotated` and inserts a new one
//! linked by `rotated_from`, keeping the original absolute expiry so rotation never extends a
//! session's lifetime. Callers rotate after login and after privilege changes. Sessions belong to a
//! principal, not a tenant, so these functions read and write through the pool.

use base64::Engine as _;
use figcraft_cloud_domain::{PrincipalId, TenantId, Uuid};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};

use crate::host_systems::HostRole;

/// Random bytes in a session token.
pub const TOKEN_BYTES: usize = 32;
/// Characters in an encoded session token (URL-safe base64 of 32 bytes, no padding).
pub const TOKEN_CHARS: usize = 43;
/// Default idle timeout (8 hours).
pub const DEFAULT_IDLE_SECS: i64 = 8 * 60 * 60;
/// Default absolute lifetime (7 days).
pub const DEFAULT_ABSOLUTE_SECS: i64 = 7 * 24 * 60 * 60;
/// Default minimum time between idle-expiry writes for one session.
pub const DEFAULT_TOUCH_INTERVAL_SECS: i64 = 60;
/// Longest absolute lifetime accepted (30 days).
pub const MAX_ABSOLUTE_SECS: i64 = 30 * 24 * 60 * 60;
/// Most sessions revoked by one [`revoke_all_for_principal`] or [`revoke_expired_sessions`] call.
pub const MAX_REVOKE_BATCH: i64 = 10_000;

/// Fixed, non-leaking categories for session failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionError {
    /// The limits are out of range.
    InvalidInput,
    /// No secure random numbers are available.
    Random,
    /// The token opens nothing (unknown, malformed, revoked, expired, or an inactive principal).
    Refused,
    /// The principal does not exist or is not active.
    InactivePrincipal,
    Database,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "session error: {self:?}")
    }
}

impl std::error::Error for SessionError {}

/// Why sessions were revoked (the `browser_sessions.revoke_reason` values).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RevokeReason {
    Logout,
    Rotated,
    MembershipRevoked,
    PrincipalSuspended,
    Operator,
    Expired,
    /// The host product that opened an embedded session ended it, was disabled, or removed the
    /// mapping the session rested on (S6-010).
    HostRevoked,
}

impl RevokeReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Logout => "logout",
            Self::Rotated => "rotated",
            Self::MembershipRevoked => "membership_revoked",
            Self::PrincipalSuspended => "principal_suspended",
            Self::Operator => "operator",
            Self::Expired => "expired",
            Self::HostRevoked => "host_revoked",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        [Self::Logout, Self::Rotated, Self::MembershipRevoked, Self::PrincipalSuspended, Self::Operator, Self::Expired, Self::HostRevoked]
            .into_iter()
            .find(|reason| reason.as_str() == value)
    }
}

/// Lifetimes applied by the repository. `figcraft_cloud_auth::session_policy::SessionPolicy`
/// holds the product policy; callers copy its values here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionLimits {
    pub idle_secs: i64,
    pub absolute_secs: i64,
    pub touch_interval_secs: i64,
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self { idle_secs: DEFAULT_IDLE_SECS, absolute_secs: DEFAULT_ABSOLUTE_SECS, touch_interval_secs: DEFAULT_TOUCH_INTERVAL_SECS }
    }
}

impl SessionLimits {
    /// True when `0 < idle ≤ absolute ≤ 30 days` and `0 ≤ touch interval < idle`.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.idle_secs > 0
            && self.idle_secs <= self.absolute_secs
            && self.absolute_secs <= MAX_ABSOLUTE_SECS
            && self.touch_interval_secs >= 0
            && self.touch_interval_secs < self.idle_secs
    }
}

/// A session just created: its id, and its token, which is never readable again.
#[derive(Clone, Eq, PartialEq)]
pub struct CreatedSession {
    pub id: Uuid,
    pub token: String,
    /// Seconds until the absolute expiry (for the cookie's `Max-Age`).
    pub absolute_remaining_secs: i64,
}

impl std::fmt::Debug for CreatedSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CreatedSession")
            .field("id", &self.id)
            .field("token", &"[REDACTED]")
            .field("absolute_remaining_secs", &self.absolute_remaining_secs)
            .finish()
    }
}

/// What an embedded session is bound to: the host that opened it, the one document it may reach
/// and the most it may do there (S6-010). The binding lives on the session row, survives rotation
/// and cannot be changed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmbedBinding {
    pub tenant_id: TenantId,
    pub system_id: String,
    pub document_id: Uuid,
    pub role: HostRole,
}

/// Longest an embedded session lives, whatever the deployment's browser-session policy says (the
/// host issues a new token to continue): 8 hours.
pub const MAX_EMBED_SESSION_SECS: i64 = 8 * 60 * 60;

/// What a live token resolves to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedSession {
    pub id: Uuid,
    pub principal_id: PrincipalId,
    /// Seconds since this session row was created (callers rotate past their rotation interval).
    pub age_secs: i64,
    /// Seconds until the absolute expiry.
    pub absolute_remaining_secs: i64,
    /// Set for a session opened by an embed-token exchange.
    pub embed: Option<EmbedBinding>,
    /// An administrator set this person's password: until they choose their own, the API accepts
    /// this session only on the routes that change the password or sign out (TS-013).
    pub password_change_required: bool,
}

/// The SHA-256 stored for `token`.
#[must_use]
pub fn session_token_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// 32 random bytes as URL-safe base64 without padding (43 characters).
pub(crate) fn random_token() -> Option<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    rustls::crypto::ring::default_provider().secure_random.fill(&mut bytes).ok()?;
    Some(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// True for exactly 43 URL-safe base64 characters.
pub(crate) fn well_formed_token(token: &str) -> bool {
    token.len() == TOKEN_CHARS && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Constant-time equality for equal-length secrets (or their hashes).
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let difference = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(difference) == 0
}

type SessionRow = (String, String, i64, i64, bool, Option<String>, Option<String>, Option<String>, Option<String>, bool);

const LIVE_SESSION_SQL: &str = concat!(
    "SELECT s.id::text, s.principal_id::text, ",
    "extract(epoch FROM now() - s.created_at)::bigint, ",
    "extract(epoch FROM s.absolute_expires_at - now())::bigint, ",
    "s.last_seen_at <= now() - make_interval(secs => $2::bigint), ",
    "s.embed_tenant_id::text, s.embed_system_id, s.embed_document_id::text, s.embed_role, s.password_change_required ",
    "FROM browser_sessions s JOIN principals p ON p.id = s.principal_id ",
    "WHERE s.token_hash = $1 AND s.revoked_at IS NULL AND s.idle_expires_at > now() ",
    "AND s.absolute_expires_at > now() AND p.state = 'active'"
);

/// [`create_session`] for a person who signed in with a temporary (administrator-set) password: the
/// session works only for changing the password and signing out until they choose their own.
pub async fn create_password_change_session(pool: &PgPool, principal_id: PrincipalId, limits: SessionLimits) -> Result<CreatedSession, SessionError> {
    let created = create_session(pool, principal_id, limits).await?;
    sqlx::query("UPDATE browser_sessions SET password_change_required = TRUE WHERE id = $1::uuid")
        .bind(created.id.to_string())
        .execute(pool)
        .await
        .map_err(|_| SessionError::Database)?;
    Ok(created)
}

/// Create a session for `principal_id`, which must be an `active` principal.
pub async fn create_session(pool: &PgPool, principal_id: PrincipalId, limits: SessionLimits) -> Result<CreatedSession, SessionError> {
    if !limits.is_valid() {
        return Err(SessionError::InvalidInput);
    }
    let token = random_token().ok_or(SessionError::Random)?;
    let id = Uuid::new_v4();
    let inserted = sqlx::query(concat!(
        "INSERT INTO browser_sessions (id, principal_id, token_hash, idle_expires_at, absolute_expires_at) ",
        "SELECT $1::uuid, p.id, $3, now() + make_interval(secs => $4::bigint), now() + make_interval(secs => $5::bigint) ",
        "FROM principals p WHERE p.id = $2::uuid AND p.state = 'active'"
    ))
    .bind(id.to_string())
    .bind(principal_id.to_string())
    .bind(session_token_hash(&token).to_vec())
    .bind(limits.idle_secs)
    .bind(limits.absolute_secs)
    .execute(pool)
    .await
    .map_err(|_| SessionError::Database)?;
    if inserted.rows_affected() != 1 {
        return Err(SessionError::InactivePrincipal);
    }
    Ok(CreatedSession { id, token, absolute_remaining_secs: limits.absolute_secs })
}

/// Create a session for `principal_id` that is bound to `binding` (S6-010): it may only reach the
/// bound document of the bound tenant, at most at the bound role, and ends when the host revokes
/// it. Both lifetimes are capped at [`MAX_EMBED_SESSION_SECS`].
pub async fn create_embedded_session(
    pool: &PgPool,
    principal_id: PrincipalId,
    binding: &EmbedBinding,
    limits: SessionLimits,
) -> Result<CreatedSession, SessionError> {
    if !limits.is_valid() || binding.system_id.is_empty() || binding.system_id.len() > 128 {
        return Err(SessionError::InvalidInput);
    }
    let absolute = limits.absolute_secs.min(MAX_EMBED_SESSION_SECS);
    let idle = limits.idle_secs.min(absolute);
    let token = random_token().ok_or(SessionError::Random)?;
    let id = Uuid::new_v4();
    let inserted = sqlx::query(concat!(
        "INSERT INTO browser_sessions (id, principal_id, token_hash, idle_expires_at, absolute_expires_at, ",
        "embed_tenant_id, embed_system_id, embed_document_id, embed_role) ",
        "SELECT $1::uuid, p.id, $3, now() + make_interval(secs => $4::bigint), now() + make_interval(secs => $5::bigint), ",
        "$6::uuid, $7, $8::uuid, $9 FROM principals p WHERE p.id = $2::uuid AND p.state = 'active'"
    ))
    .bind(id.to_string())
    .bind(principal_id.to_string())
    .bind(session_token_hash(&token).to_vec())
    .bind(idle)
    .bind(absolute)
    .bind(binding.tenant_id.to_string())
    .bind(&binding.system_id)
    .bind(binding.document_id.to_string())
    .bind(binding.role.as_str())
    .execute(pool)
    .await
    .map_err(|_| SessionError::Database)?;
    if inserted.rows_affected() != 1 {
        return Err(SessionError::InactivePrincipal);
    }
    Ok(CreatedSession { id, token, absolute_remaining_secs: absolute })
}

/// Which embedded sessions of a host to end.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmbedScope<'a> {
    /// Every session the host opened.
    Host,
    /// Sessions the host opened on one document.
    Document(Uuid),
    /// Sessions the host opened for one principal.
    Principal(&'a Uuid),
}

/// End the live embedded sessions of host `system_id` in `tenant` that match `scope`, in the
/// caller's transaction (so the revocation commits with the change that caused it). The relay
/// notices through its ordinary session check. Returns how many sessions ended.
pub async fn revoke_embedded_sessions(
    connection: &mut PgConnection,
    tenant: TenantId,
    system_id: &str,
    scope: EmbedScope<'_>,
) -> Result<u64, SessionError> {
    let (document, principal) = match scope {
        EmbedScope::Host => (None, None),
        EmbedScope::Document(document) => (Some(document.to_string()), None),
        EmbedScope::Principal(principal) => (None, Some(principal.to_string())),
    };
    let result = sqlx::query(concat!(
        "UPDATE browser_sessions SET revoked_at = now(), revoke_reason = 'host_revoked' ",
        "WHERE embed_tenant_id = $1::uuid AND embed_system_id = $2 AND revoked_at IS NULL ",
        "AND ($3::uuid IS NULL OR embed_document_id = $3::uuid) AND ($4::uuid IS NULL OR principal_id = $4::uuid)"
    ))
    .bind(tenant.to_string())
    .bind(system_id)
    .bind(document)
    .bind(principal)
    .execute(connection)
    .await
    .map_err(|_| SessionError::Database)?;
    Ok(result.rows_affected())
}

fn parse_row(row: SessionRow) -> Result<(ResolvedSession, bool), SessionError> {
    let (
        id,
        principal_id,
        age_secs,
        absolute_remaining_secs,
        stale,
        embed_tenant,
        embed_system,
        embed_document,
        embed_role,
        password_change_required,
    ) = row;
    // The columns are all set or all null (a table constraint); anything else is a broken row.
    let embed = match (embed_tenant, embed_system, embed_document, embed_role) {
        (None, None, None, None) => None,
        (Some(tenant), Some(system_id), Some(document), Some(role)) => Some(EmbedBinding {
            tenant_id: tenant.parse().map_err(|_| SessionError::Database)?,
            system_id,
            document_id: document.parse().map_err(|_| SessionError::Database)?,
            role: HostRole::parse(&role).ok_or(SessionError::Database)?,
        }),
        _ => return Err(SessionError::Database),
    };
    Ok((
        ResolvedSession {
            id: id.parse().map_err(|_| SessionError::Database)?,
            principal_id: principal_id.parse().map_err(|_| SessionError::Database)?,
            age_secs: age_secs.max(0),
            absolute_remaining_secs: absolute_remaining_secs.max(0),
            embed,
            password_change_required,
        },
        stale,
    ))
}

/// Resolve a presented token to its live session, sliding the idle expiry when the last write is
/// older than the touch interval. Anything not live is [`SessionError::Refused`].
pub async fn resolve_session(pool: &PgPool, token: &str, limits: SessionLimits) -> Result<ResolvedSession, SessionError> {
    if !limits.is_valid() {
        return Err(SessionError::InvalidInput);
    }
    if !well_formed_token(token) {
        return Err(SessionError::Refused);
    }
    let hash = session_token_hash(token).to_vec();
    let row: Option<SessionRow> = sqlx::query_as(LIVE_SESSION_SQL)
        .bind(&hash)
        .bind(limits.touch_interval_secs)
        .fetch_optional(pool)
        .await
        .map_err(|_| SessionError::Database)?;
    let (session, stale) = parse_row(row.ok_or(SessionError::Refused)?)?;
    if stale {
        // Bounded write: only when the last touch is older than the interval, and still guarded
        // by liveness so a concurrent revoke is never undone.
        sqlx::query(concat!(
            "UPDATE browser_sessions SET last_seen_at = now(), ",
            "idle_expires_at = least(absolute_expires_at, now() + make_interval(secs => $2::bigint)) ",
            "WHERE id = $1::uuid AND revoked_at IS NULL AND idle_expires_at > now() ",
            "AND last_seen_at <= now() - make_interval(secs => $3::bigint)"
        ))
        .bind(session.id.to_string())
        .bind(limits.idle_secs)
        .bind(limits.touch_interval_secs)
        .execute(pool)
        .await
        .map_err(|_| SessionError::Database)?;
    }
    Ok(session)
}

/// Replace the live session named by `token` with a new one for the same principal: the old row
/// is revoked with reason `rotated` and the new row records it in `rotated_from`. The absolute
/// expiry is inherited. A token that is not live is [`SessionError::Refused`]; of two concurrent
/// rotations of one token exactly one succeeds.
pub async fn rotate_session(pool: &PgPool, token: &str, limits: SessionLimits) -> Result<CreatedSession, SessionError> {
    if !limits.is_valid() {
        return Err(SessionError::InvalidInput);
    }
    if !well_formed_token(token) {
        return Err(SessionError::Refused);
    }
    let new_token = random_token().ok_or(SessionError::Random)?;
    let new_id = Uuid::new_v4();
    let mut transaction = pool.begin().await.map_err(|_| SessionError::Database)?;
    // Lock order shared with `revoke_all_for_principal`: the principal first (`FOR SHARE` here,
    // `FOR UPDATE` there), then sessions, so a rotation and a revoke-all serialise without
    // deadlocking, and the revoke-all always sees the session a rotation inserts.
    sqlx::query("SELECT 1 FROM principals WHERE id = (SELECT principal_id FROM browser_sessions WHERE token_hash = $1) FOR SHARE")
        .bind(session_token_hash(token).to_vec())
        .execute(&mut *transaction)
        .await
        .map_err(|_| SessionError::Database)?;
    let old: Option<(String, i64)> = sqlx::query_as(concat!(
        "SELECT s.id::text, extract(epoch FROM s.absolute_expires_at - now())::bigint ",
        "FROM browser_sessions s JOIN principals p ON p.id = s.principal_id ",
        "WHERE s.token_hash = $1 AND s.revoked_at IS NULL AND s.idle_expires_at > now() ",
        "AND s.absolute_expires_at > now() AND p.state = 'active' FOR UPDATE OF s"
    ))
    .bind(session_token_hash(token).to_vec())
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|_| SessionError::Database)?;
    let (old_id, remaining) = old.ok_or(SessionError::Refused)?;
    sqlx::query("UPDATE browser_sessions SET revoked_at = now(), revoke_reason = 'rotated' WHERE id = $1::uuid")
        .bind(&old_id)
        .execute(&mut *transaction)
        .await
        .map_err(|_| SessionError::Database)?;
    sqlx::query(concat!(
        "INSERT INTO browser_sessions (id, principal_id, token_hash, rotated_from, idle_expires_at, absolute_expires_at, ",
        "embed_tenant_id, embed_system_id, embed_document_id, embed_role, password_change_required) ",
        "SELECT $1::uuid, o.principal_id, $2, o.id, ",
        "least(o.absolute_expires_at, now() + make_interval(secs => $4::bigint)), o.absolute_expires_at, ",
        "o.embed_tenant_id, o.embed_system_id, o.embed_document_id, o.embed_role, o.password_change_required ",
        "FROM browser_sessions o WHERE o.id = $3::uuid"
    ))
    .bind(new_id.to_string())
    .bind(session_token_hash(&new_token).to_vec())
    .bind(&old_id)
    .bind(limits.idle_secs)
    .execute(&mut *transaction)
    .await
    .map_err(|_| SessionError::Database)?;
    transaction.commit().await.map_err(|_| SessionError::Database)?;
    Ok(CreatedSession { id: new_id, token: new_token, absolute_remaining_secs: remaining.max(0) })
}

/// Revoke the session named by `token` with reason `logout`: true when it was unrevoked.
/// Malformed and unknown tokens are simply false (logout is idempotent).
pub async fn logout(pool: &PgPool, token: &str) -> Result<bool, SessionError> {
    if !well_formed_token(token) {
        return Ok(false);
    }
    let result = sqlx::query("UPDATE browser_sessions SET revoked_at = now(), revoke_reason = 'logout' WHERE token_hash = $1 AND revoked_at IS NULL")
        .bind(session_token_hash(token).to_vec())
        .execute(pool)
        .await
        .map_err(|_| SessionError::Database)?;
    Ok(result.rows_affected() == 1)
}

/// Revoke every unrevoked session of `principal_id` (at most [`MAX_REVOKE_BATCH`] per call; call
/// again while it returns that many). Returns how many were revoked.
pub async fn revoke_all_for_principal(pool: &PgPool, principal_id: PrincipalId, reason: RevokeReason) -> Result<u64, SessionError> {
    // Serialise with `rotate_session`, which holds the principal `FOR SHARE` while it replaces a
    // session: once this lock is granted every concurrent rotation has committed, and the UPDATE
    // below (a new statement, so a new READ COMMITTED snapshot) sees the session it inserted.
    let mut transaction = pool.begin().await.map_err(|_| SessionError::Database)?;
    sqlx::query("SELECT 1 FROM principals WHERE id = $1::uuid FOR UPDATE")
        .bind(principal_id.to_string())
        .execute(&mut *transaction)
        .await
        .map_err(|_| SessionError::Database)?;
    let result = sqlx::query(concat!(
        "UPDATE browser_sessions SET revoked_at = now(), revoke_reason = $2 WHERE id IN (",
        "SELECT id FROM browser_sessions WHERE principal_id = $1::uuid AND revoked_at IS NULL LIMIT $3)"
    ))
    .bind(principal_id.to_string())
    .bind(reason.as_str())
    .bind(MAX_REVOKE_BATCH)
    .execute(&mut *transaction)
    .await
    .map_err(|_| SessionError::Database)?;
    transaction.commit().await.map_err(|_| SessionError::Database)?;
    Ok(result.rows_affected())
}

/// Most [`revoke_all_for_principal`] batches one [`revoke_all_for_identity`] call runs.
pub const MAX_IDENTITY_REVOKE_BATCHES: u32 = 8;

/// Revoke every unrevoked session of the principal linked to `(issuer, subject)` in
/// `identity_links` (S4-025 back-channel logout). Sessions are not tied to the issuer that
/// signed their login, so this ends all of that principal's browser sessions. An unknown
/// identity revokes nothing (`Ok(0)`); out-of-range input is [`SessionError::InvalidInput`].
/// Runs at most [`MAX_IDENTITY_REVOKE_BATCHES`] batches. Returns how many were revoked.
pub async fn revoke_all_for_identity(pool: &PgPool, issuer: &str, subject: &str, reason: RevokeReason) -> Result<u64, SessionError> {
    let in_range = |value: &str, max: usize| !value.is_empty() && value.chars().count() <= max;
    if !in_range(issuer, 2048) || !in_range(subject, 1024) {
        return Err(SessionError::InvalidInput);
    }
    let principal: Option<(String,)> = sqlx::query_as("SELECT principal_id::text FROM identity_links WHERE issuer = $1 AND subject = $2")
        .bind(issuer)
        .bind(subject)
        .fetch_optional(pool)
        .await
        .map_err(|_| SessionError::Database)?;
    let Some((principal,)) = principal else {
        return Ok(0);
    };
    let principal: PrincipalId = principal.parse().map_err(|_| SessionError::Database)?;
    let mut total = 0u64;
    for _ in 0..MAX_IDENTITY_REVOKE_BATCHES {
        let revoked = revoke_all_for_principal(pool, principal, reason).await?;
        total = total.saturating_add(revoked);
        if revoked < u64::try_from(MAX_REVOKE_BATCH).unwrap_or(u64::MAX) {
            break;
        }
    }
    Ok(total)
}

/// Mark up to `limit` (capped at [`MAX_REVOKE_BATCH`]) unrevoked but idle- or absolute-expired
/// sessions revoked with reason `expired` (housekeeping; resolution already refuses them).
pub async fn revoke_expired_sessions(pool: &PgPool, limit: i64) -> Result<u64, SessionError> {
    let limit = limit.clamp(0, MAX_REVOKE_BATCH);
    let result = sqlx::query(concat!(
        "UPDATE browser_sessions SET revoked_at = now(), revoke_reason = 'expired' WHERE id IN (",
        "SELECT id FROM browser_sessions WHERE revoked_at IS NULL ",
        "AND (idle_expires_at <= now() OR absolute_expires_at <= now()) LIMIT $1)"
    ))
    .bind(limit)
    .execute(pool)
    .await
    .map_err(|_| SessionError::Database)?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_43_url_safe_characters_and_distinct() {
        let (a, b) = (random_token().unwrap(), random_token().unwrap());
        assert!(well_formed_token(&a) && well_formed_token(&b));
        assert_ne!(a, b);
    }

    fn row(embed: [Option<&str>; 4]) -> SessionRow {
        let [tenant, system, document, role] = embed.map(|value| value.map(str::to_owned));
        (
            "e1000000-0000-4000-8000-0000000000a1".to_owned(),
            "e1000000-0000-4000-8000-0000000000a2".to_owned(),
            5,
            600,
            false,
            tenant,
            system,
            document,
            role,
            false,
        )
    }

    #[test]
    fn a_session_row_is_embedded_only_when_its_whole_binding_is_present() {
        const T: &str = "e1000000-0000-4000-8000-0000000000b1";
        const D: &str = "e1000000-0000-4000-8000-0000000000b2";
        let (plain, _) = parse_row(row([None; 4])).unwrap();
        assert_eq!(plain.embed, None);
        let (bound, _) = parse_row(row([Some(T), Some("host-a"), Some(D), Some("viewer")])).unwrap();
        let embed = bound.embed.unwrap();
        assert_eq!((embed.system_id.as_str(), embed.role), ("host-a", HostRole::Viewer));
        assert_eq!((embed.tenant_id.to_string(), embed.document_id.to_string()), (T.to_owned(), D.to_owned()));
        // A half-set binding, an unknown role or a bad id is a broken row, never a plain session.
        for broken in [
            [Some(T), None, None, None],
            [None, Some("host-a"), None, None],
            [Some(T), Some("host-a"), Some(D), None],
            [Some(T), Some("host-a"), Some(D), Some("owner")],
            [Some("nope"), Some("host-a"), Some(D), Some("editor")],
        ] {
            assert_eq!(parse_row(row(broken)).unwrap_err(), SessionError::Database, "{broken:?}");
        }
    }

    #[test]
    fn host_revoked_is_a_known_revoke_reason() {
        assert_eq!(RevokeReason::parse("host_revoked"), Some(RevokeReason::HostRevoked));
        assert_eq!(RevokeReason::HostRevoked.as_str(), "host_revoked");
    }

    #[test]
    fn malformed_tokens_are_rejected() {
        for bad in [String::new(), "a".repeat(42), "a".repeat(44), format!("{}+", "a".repeat(42)), format!("{}é", "a".repeat(41))] {
            assert!(!well_formed_token(&bad), "{bad}");
        }
    }

    #[test]
    fn constant_time_eq_matches_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn limits_are_bounded() {
        assert!(SessionLimits::default().is_valid());
        let base = SessionLimits::default();
        for bad in [
            SessionLimits { idle_secs: 0, ..base },
            SessionLimits { idle_secs: base.absolute_secs + 1, ..base },
            SessionLimits { absolute_secs: MAX_ABSOLUTE_SECS + 1, ..base },
            SessionLimits { touch_interval_secs: -1, ..base },
            SessionLimits { touch_interval_secs: base.idle_secs, ..base },
            SessionLimits { idle_secs: i64::MIN, absolute_secs: i64::MAX, touch_interval_secs: 0 },
        ] {
            assert!(!bad.is_valid(), "{bad:?}");
        }
    }

    #[test]
    fn reasons_round_trip() {
        for reason in [
            RevokeReason::Logout,
            RevokeReason::Rotated,
            RevokeReason::MembershipRevoked,
            RevokeReason::PrincipalSuspended,
            RevokeReason::Operator,
            RevokeReason::Expired,
        ] {
            assert_eq!(RevokeReason::parse(reason.as_str()), Some(reason));
        }
        assert_eq!(RevokeReason::parse("bogus"), None);
    }

    #[test]
    fn created_sessions_never_print_their_token() {
        let session = CreatedSession { id: Uuid::nil(), token: "top-secret".into(), absolute_remaining_secs: 1 };
        assert!(!format!("{session:?}").contains("top-secret"));
    }
}

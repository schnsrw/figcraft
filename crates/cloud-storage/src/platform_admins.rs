//! Platform admins, their sessions and the one-time bootstrap token (S6-045).
//!
//! A platform admin is a person who runs the deployment. It is a separate record from a workspace
//! user: no `principals` row, no membership in any workspace, no way to sign in to a workspace
//! with it. Everything here runs in a [`PlatformTransaction`]; password hashing and TOTP code
//! checking are done by the caller (this module stores the opaque Argon2id string and the sealed
//! TOTP secret).
//!
//! Session tokens are 32 random bytes (the same encoding as workspace sessions) of which only the
//! SHA-256 is stored. A session starts with the second factor not done; the platform API accepts
//! only sessions with `second_factor_done`.

use figcraft_cloud_domain::Uuid;
use sha2::{Digest, Sha256};

use crate::platform::{PlatformError, PlatformTransaction, database_error};
use crate::sessions::{constant_time_eq, random_token, well_formed_token};

/// Longest bootstrap token lifetime (24 hours).
pub const MAX_BOOTSTRAP_LIFETIME_SECS: i64 = 24 * 60 * 60;
/// Most platform admins that can exist (a bound, not a target).
pub const MAX_ADMINS: i64 = 100;
/// Platform session lifetimes are bounded by this absolute maximum (12 hours).
pub const MAX_PLATFORM_SESSION_SECS: i64 = 12 * 60 * 60;

/// Lowercase letters, digits and `@ . _ + -`, 3 to 254 characters. Returns the normalized login.
#[must_use]
pub fn normalize_login(login: &str) -> Option<String> {
    let login = login.trim().to_ascii_lowercase();
    ((3..=254).contains(&login.len())
        && login.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'@' | b'.' | b'_' | b'+' | b'-')))
    .then_some(login)
}

fn display_name(name: &str) -> Option<&str> {
    let name = name.trim();
    let count = name.chars().count();
    ((1..=255).contains(&count) && !name.chars().any(char::is_control)).then_some(name)
}

/// Whether an admin may sign in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdminState {
    Active,
    Disabled,
}

impl AdminState {
    fn parse(value: &str) -> Result<Self, PlatformError> {
        match value {
            "active" => Ok(Self::Active),
            "disabled" => Ok(Self::Disabled),
            _ => Err(PlatformError::CorruptRow),
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Disabled => "disabled",
        }
    }
}

/// What the API may show about an admin. Never the hash or the TOTP secret.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlatformAdmin {
    pub id: Uuid,
    pub login: String,
    pub display_name: String,
    pub state: AdminState,
    pub has_password: bool,
    pub totp_confirmed: bool,
    pub created_at: i64,
    pub last_login_at: Option<i64>,
}

/// The secrets and counters sign-in needs. `Debug` redacts them.
#[derive(Clone, Eq, PartialEq)]
pub struct AdminCredentials {
    pub admin: PlatformAdmin,
    pub password_hash: Option<String>,
    pub totp_secret: Option<Vec<u8>>,
    pub failed_logins: i32,
    /// Seconds until the lock ends, `0` when not locked.
    pub locked_for_secs: i64,
}

impl std::fmt::Debug for AdminCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("AdminCredentials").field("admin", &self.admin).field("secrets", &"[REDACTED]").finish_non_exhaustive()
    }
}

type AdminRow = (String, String, String, String, bool, bool, i64, Option<i64>);

macro_rules! admin_columns {
    () => {
        "id::text, login, display_name, state, password_hash IS NOT NULL, totp_confirmed_at IS NOT NULL, \
         floor(extract(epoch FROM created_at))::bigint, floor(extract(epoch FROM last_login_at))::bigint"
    };
}

fn admin(row: AdminRow) -> Result<PlatformAdmin, PlatformError> {
    Ok(PlatformAdmin {
        id: row.0.parse().map_err(|_| PlatformError::CorruptRow)?,
        login: row.1,
        display_name: row.2,
        state: AdminState::parse(&row.3)?,
        has_password: row.4,
        totp_confirmed: row.5,
        created_at: row.6,
        last_login_at: row.7,
    })
}

/// Issue (or replace) the one-time bootstrap token. Refused when a platform admin already exists.
/// The token is returned once; only its hash is stored.
pub async fn issue_bootstrap_token(transaction: &mut PlatformTransaction<'_>, lifetime_secs: i64) -> Result<String, PlatformError> {
    if !(60..=MAX_BOOTSTRAP_LIFETIME_SECS).contains(&lifetime_secs) {
        return Err(PlatformError::Invalid);
    }
    let existing: i64 =
        sqlx::query_scalar("SELECT count(*) FROM platform_admins").fetch_one(transaction.connection()).await.map_err(|e| database_error(&e))?;
    if existing > 0 {
        return Err(PlatformError::Refused);
    }
    let token = random_token().ok_or(PlatformError::Database)?;
    sqlx::query(
        "INSERT INTO platform_bootstrap (singleton, token_hash, expires_at, consumed_at) VALUES (TRUE, $1, now() + make_interval(secs => $2::bigint), NULL) \
         ON CONFLICT (singleton) DO UPDATE SET token_hash = EXCLUDED.token_hash, expires_at = EXCLUDED.expires_at, consumed_at = NULL",
    )
    .bind(Sha256::digest(token.as_bytes()).to_vec())
    .bind(lifetime_secs)
    .execute(transaction.connection())
    .await
    .map_err(|e| database_error(&e))?;
    Ok(token)
}

/// Create the first platform admin by spending the bootstrap token. The token row is locked, so
/// two concurrent attempts cannot both succeed. Every failure is `Refused`.
pub async fn bootstrap_first_admin(
    transaction: &mut PlatformTransaction<'_>,
    token: &str,
    login: &str,
    name: &str,
    password_hash: &str,
) -> Result<PlatformAdmin, PlatformError> {
    let login = normalize_login(login).ok_or(PlatformError::Invalid)?;
    let name = display_name(name).ok_or(PlatformError::Invalid)?;
    let row: Option<(Vec<u8>, bool)> =
        sqlx::query_as("SELECT token_hash, consumed_at IS NULL AND expires_at > now() FROM platform_bootstrap WHERE singleton FOR UPDATE")
            .fetch_optional(transaction.connection())
            .await
            .map_err(|e| database_error(&e))?;
    let presented = Sha256::digest(token.as_bytes());
    let live = matches!(&row, Some((stored, true)) if well_formed_token(token) && constant_time_eq(stored, &presented));
    let existing: i64 =
        sqlx::query_scalar("SELECT count(*) FROM platform_admins").fetch_one(transaction.connection()).await.map_err(|e| database_error(&e))?;
    if !live || existing > 0 {
        return Err(PlatformError::Refused);
    }
    let created = insert_admin(transaction, &login, name, Some(password_hash), None).await?;
    sqlx::query("UPDATE platform_bootstrap SET consumed_at = now() WHERE singleton")
        .execute(transaction.connection())
        .await
        .map_err(|e| database_error(&e))?;
    Ok(created)
}

async fn insert_admin(
    transaction: &mut PlatformTransaction<'_>,
    login: &str,
    name: &str,
    password_hash: Option<&str>,
    created_by: Option<Uuid>,
) -> Result<PlatformAdmin, PlatformError> {
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM platform_admins").fetch_one(transaction.connection()).await.map_err(|e| database_error(&e))?;
    if count >= MAX_ADMINS {
        return Err(PlatformError::Refused);
    }
    let row: AdminRow = sqlx::query_as(concat!(
        "INSERT INTO platform_admins (id, login, display_name, password_hash, created_by) VALUES ($1::uuid, $2, $3, $4, $5::uuid) RETURNING ",
        admin_columns!()
    ))
    .bind(Uuid::new_v4().to_string())
    .bind(login)
    .bind(name)
    .bind(password_hash)
    .bind(created_by.map(|id| id.to_string()))
    .fetch_one(transaction.connection())
    .await
    .map_err(|e| database_error(&e))?;
    admin(row)
}

/// Create a platform admin (by an existing admin). A conflicting login is `Conflict`.
pub async fn create_admin(
    transaction: &mut PlatformTransaction<'_>,
    created_by: Option<Uuid>,
    login: &str,
    name: &str,
    password_hash: &str,
) -> Result<PlatformAdmin, PlatformError> {
    let login = normalize_login(login).ok_or(PlatformError::Invalid)?;
    let name = display_name(name).ok_or(PlatformError::Invalid)?;
    insert_admin(transaction, &login, name, Some(password_hash), created_by).await
}

/// All platform admins, oldest first (at most [`MAX_ADMINS`]).
pub async fn list_admins(transaction: &mut PlatformTransaction<'_>) -> Result<Vec<PlatformAdmin>, PlatformError> {
    let rows: Vec<AdminRow> = sqlx::query_as(concat!("SELECT ", admin_columns!(), " FROM platform_admins ORDER BY created_at, id LIMIT 100"))
        .fetch_all(transaction.connection())
        .await
        .map_err(|e| database_error(&e))?;
    rows.into_iter().map(admin).collect()
}

pub async fn get_admin(transaction: &mut PlatformTransaction<'_>, id: Uuid) -> Result<Option<PlatformAdmin>, PlatformError> {
    let row: Option<AdminRow> = sqlx::query_as(concat!("SELECT ", admin_columns!(), " FROM platform_admins WHERE id = $1::uuid"))
        .bind(id.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(|e| database_error(&e))?;
    row.map(admin).transpose()
}

/// The sign-in record for a login (normalized first), or `None`.
pub async fn credentials_by_login(transaction: &mut PlatformTransaction<'_>, login: &str) -> Result<Option<AdminCredentials>, PlatformError> {
    let Some(login) = normalize_login(login) else {
        return Ok(None);
    };
    let row: Option<AdminRow> = sqlx::query_as(concat!("SELECT ", admin_columns!(), " FROM platform_admins WHERE login = $1"))
        .bind(&login)
        .fetch_optional(transaction.connection())
        .await
        .map_err(|e| database_error(&e))?;
    let Some(row) = row else {
        return Ok(None);
    };
    let admin = admin(row)?;
    let secrets: (Option<String>, Option<Vec<u8>>, i32, i64) = sqlx::query_as(
        "SELECT password_hash, totp_secret, failed_logins, \
                GREATEST(0, ceil(extract(epoch FROM COALESCE(locked_until, now()) - now())))::bigint \
         FROM platform_admins WHERE id = $1::uuid",
    )
    .bind(admin.id.to_string())
    .fetch_one(transaction.connection())
    .await
    .map_err(|e| database_error(&e))?;
    Ok(Some(AdminCredentials { admin, password_hash: secrets.0, totp_secret: secrets.1, failed_logins: secrets.2, locked_for_secs: secrets.3 }))
}

/// Count a failed sign-in; at `lock_after` consecutive failures the admin is locked for
/// `lock_secs`. Returns the new failure count.
pub async fn record_failed_login(transaction: &mut PlatformTransaction<'_>, id: Uuid, lock_after: i32, lock_secs: i64) -> Result<i32, PlatformError> {
    let (failures,): (i32,) = sqlx::query_as(
        "UPDATE platform_admins SET failed_logins = failed_logins + 1, \
             locked_until = CASE WHEN failed_logins + 1 >= $2 THEN now() + make_interval(secs => $3::bigint) ELSE locked_until END, \
             updated_at = now() WHERE id = $1::uuid RETURNING failed_logins",
    )
    .bind(id.to_string())
    .bind(lock_after)
    .bind(lock_secs)
    .fetch_one(transaction.connection())
    .await
    .map_err(|e| database_error(&e))?;
    Ok(failures)
}

/// A completed sign-in clears the failure counter and the lock.
pub async fn record_successful_login(transaction: &mut PlatformTransaction<'_>, id: Uuid) -> Result<(), PlatformError> {
    sqlx::query("UPDATE platform_admins SET failed_logins = 0, locked_until = NULL, last_login_at = now(), updated_at = now() WHERE id = $1::uuid")
        .bind(id.to_string())
        .execute(transaction.connection())
        .await
        .map_err(|e| database_error(&e))?;
    Ok(())
}

/// Revoke every live session of an admin; returns how many.
pub async fn revoke_admin_sessions(transaction: &mut PlatformTransaction<'_>, id: Uuid, reason: &str) -> Result<u64, PlatformError> {
    if !matches!(reason, "logout" | "admin_disabled" | "password_changed" | "operator") {
        return Err(PlatformError::Invalid);
    }
    let done = sqlx::query("UPDATE platform_sessions SET revoked_at = now(), revoke_reason = $2 WHERE admin_id = $1::uuid AND revoked_at IS NULL")
        .bind(id.to_string())
        .bind(reason)
        .execute(transaction.connection())
        .await
        .map_err(|e| database_error(&e))?;
    Ok(done.rows_affected())
}

/// Enable or disable an admin. Disabling revokes their sessions and is refused when it would leave
/// no active admin (`Refused`). An admin cannot be left with nobody able to manage the platform.
pub async fn set_admin_state(transaction: &mut PlatformTransaction<'_>, id: Uuid, state: AdminState) -> Result<PlatformAdmin, PlatformError> {
    // Serialize concurrent disables so the "last active admin" count cannot be raced.
    sqlx::query("SELECT id FROM platform_admins WHERE state = 'active' ORDER BY id FOR UPDATE")
        .fetch_all(transaction.connection())
        .await
        .map_err(|e| database_error(&e))?;
    let current = get_admin(transaction, id).await?.ok_or(PlatformError::NotFound)?;
    if state == AdminState::Disabled && current.state == AdminState::Active {
        let active: i64 = sqlx::query_scalar("SELECT count(*) FROM platform_admins WHERE state = 'active'")
            .fetch_one(transaction.connection())
            .await
            .map_err(|e| database_error(&e))?;
        if active <= 1 {
            return Err(PlatformError::Refused);
        }
    }
    sqlx::query("UPDATE platform_admins SET state = $2, updated_at = now() WHERE id = $1::uuid")
        .bind(id.to_string())
        .bind(state.as_str())
        .execute(transaction.connection())
        .await
        .map_err(|e| database_error(&e))?;
    if state == AdminState::Disabled {
        revoke_admin_sessions(transaction, id, "admin_disabled").await?;
    }
    get_admin(transaction, id).await?.ok_or(PlatformError::NotFound)
}

/// Replace an admin's password hash; their sessions are revoked.
pub async fn set_admin_password(transaction: &mut PlatformTransaction<'_>, id: Uuid, password_hash: &str) -> Result<(), PlatformError> {
    let done =
        sqlx::query("UPDATE platform_admins SET password_hash = $2, failed_logins = 0, locked_until = NULL, updated_at = now() WHERE id = $1::uuid")
            .bind(id.to_string())
            .bind(password_hash)
            .execute(transaction.connection())
            .await
            .map_err(|e| database_error(&e))?;
    if done.rows_affected() == 0 {
        return Err(PlatformError::NotFound);
    }
    revoke_admin_sessions(transaction, id, "password_changed").await?;
    Ok(())
}

/// Store a sealed TOTP secret for enrolment; it counts only after [`confirm_totp`].
pub async fn store_totp_secret(transaction: &mut PlatformTransaction<'_>, id: Uuid, sealed_secret: &[u8]) -> Result<(), PlatformError> {
    let done = sqlx::query("UPDATE platform_admins SET totp_secret = $2, totp_confirmed_at = NULL, updated_at = now() WHERE id = $1::uuid")
        .bind(id.to_string())
        .bind(sealed_secret)
        .execute(transaction.connection())
        .await
        .map_err(|e| database_error(&e))?;
    if done.rows_affected() == 0 { Err(PlatformError::NotFound) } else { Ok(()) }
}

/// Accept TOTP step `step` once: true when it is newer than every step accepted before (so the
/// same or an older code is refused), false otherwise. Atomic.
pub async fn accept_totp_step(transaction: &mut PlatformTransaction<'_>, id: Uuid, step: i64) -> Result<bool, PlatformError> {
    let done = sqlx::query("UPDATE platform_admins SET totp_last_step = $2 WHERE id = $1::uuid AND totp_last_step < $2")
        .bind(id.to_string())
        .bind(step)
        .execute(transaction.connection())
        .await
        .map_err(|e| database_error(&e))?;
    Ok(done.rows_affected() == 1)
}

/// Mark the stored secret as proven by a verified code.
pub async fn confirm_totp(transaction: &mut PlatformTransaction<'_>, id: Uuid) -> Result<(), PlatformError> {
    let done =
        sqlx::query("UPDATE platform_admins SET totp_confirmed_at = now(), updated_at = now() WHERE id = $1::uuid AND totp_secret IS NOT NULL")
            .bind(id.to_string())
            .execute(transaction.connection())
            .await
            .map_err(|e| database_error(&e))?;
    if done.rows_affected() == 0 { Err(PlatformError::NotFound) } else { Ok(()) }
}

/// Platform session lifetimes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlatformSessionLimits {
    pub idle_secs: i64,
    pub absolute_secs: i64,
    pub touch_interval_secs: i64,
}

impl Default for PlatformSessionLimits {
    /// 15 minutes idle, 4 hours absolute: shorter than a workspace session.
    fn default() -> Self {
        Self { idle_secs: 15 * 60, absolute_secs: 4 * 60 * 60, touch_interval_secs: 30 }
    }
}

impl PlatformSessionLimits {
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.idle_secs > 0
            && self.idle_secs <= self.absolute_secs
            && self.absolute_secs <= MAX_PLATFORM_SESSION_SECS
            && self.touch_interval_secs >= 0
            && self.touch_interval_secs < self.idle_secs
    }
}

/// A new session; the token is shown once.
#[derive(Clone, Eq, PartialEq)]
pub struct CreatedPlatformSession {
    pub id: Uuid,
    pub token: String,
    pub absolute_remaining_secs: i64,
}

impl std::fmt::Debug for CreatedPlatformSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("CreatedPlatformSession").field("id", &self.id).field("token", &"[REDACTED]").finish_non_exhaustive()
    }
}

/// Create a session for an active admin. `second_factor_done` is false until the TOTP step passes.
pub async fn create_session(
    transaction: &mut PlatformTransaction<'_>,
    admin_id: Uuid,
    second_factor_done: bool,
    limits: PlatformSessionLimits,
) -> Result<CreatedPlatformSession, PlatformError> {
    if !limits.is_valid() {
        return Err(PlatformError::Invalid);
    }
    let token = random_token().ok_or(PlatformError::Database)?;
    let id = Uuid::new_v4();
    let done = sqlx::query(
        "INSERT INTO platform_sessions (id, admin_id, token_hash, second_factor_done, idle_expires_at, absolute_expires_at) \
         SELECT $1::uuid, a.id, $3, $4, now() + make_interval(secs => $5::bigint), now() + make_interval(secs => $6::bigint) \
         FROM platform_admins a WHERE a.id = $2::uuid AND a.state = 'active'",
    )
    .bind(id.to_string())
    .bind(admin_id.to_string())
    .bind(Sha256::digest(token.as_bytes()).to_vec())
    .bind(second_factor_done)
    .bind(limits.idle_secs)
    .bind(limits.absolute_secs)
    .execute(transaction.connection())
    .await
    .map_err(|e| database_error(&e))?;
    if done.rows_affected() != 1 {
        return Err(PlatformError::Refused);
    }
    Ok(CreatedPlatformSession { id, token, absolute_remaining_secs: limits.absolute_secs })
}

/// What a live platform session token resolves to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedPlatformSession {
    pub id: Uuid,
    pub admin_id: Uuid,
    pub login: String,
    pub second_factor_done: bool,
}

/// Resolve a presented token, sliding the idle expiry at most once per touch interval. Anything
/// not live (unknown, malformed, revoked, expired, disabled admin) is `Refused`.
pub async fn resolve_session(
    transaction: &mut PlatformTransaction<'_>,
    token: &str,
    limits: PlatformSessionLimits,
) -> Result<ResolvedPlatformSession, PlatformError> {
    if !limits.is_valid() {
        return Err(PlatformError::Invalid);
    }
    if !well_formed_token(token) {
        return Err(PlatformError::Refused);
    }
    let row: Option<(String, String, String, bool, bool)> = sqlx::query_as(
        "SELECT s.id::text, a.id::text, a.login, s.second_factor_done, s.last_seen_at <= now() - make_interval(secs => $2::bigint) \
         FROM platform_sessions s JOIN platform_admins a ON a.id = s.admin_id \
         WHERE s.token_hash = $1 AND s.revoked_at IS NULL AND s.idle_expires_at > now() AND s.absolute_expires_at > now() AND a.state = 'active'",
    )
    .bind(Sha256::digest(token.as_bytes()).to_vec())
    .bind(limits.touch_interval_secs)
    .fetch_optional(transaction.connection())
    .await
    .map_err(|e| database_error(&e))?;
    let (id, admin_id, login, second_factor_done, stale) = row.ok_or(PlatformError::Refused)?;
    if stale {
        sqlx::query(
            "UPDATE platform_sessions SET last_seen_at = now(), idle_expires_at = least(absolute_expires_at, now() + make_interval(secs => $2::bigint)) \
             WHERE id = $1::uuid AND revoked_at IS NULL",
        )
        .bind(&id)
        .bind(limits.idle_secs)
        .execute(transaction.connection())
        .await
        .map_err(|e| database_error(&e))?;
    }
    Ok(ResolvedPlatformSession {
        id: id.parse().map_err(|_| PlatformError::CorruptRow)?,
        admin_id: admin_id.parse().map_err(|_| PlatformError::CorruptRow)?,
        login,
        second_factor_done,
    })
}

/// Mark a session as past the second factor (after a verified TOTP code).
pub async fn mark_second_factor_done(transaction: &mut PlatformTransaction<'_>, session_id: Uuid) -> Result<(), PlatformError> {
    let done = sqlx::query("UPDATE platform_sessions SET second_factor_done = TRUE WHERE id = $1::uuid AND revoked_at IS NULL")
        .bind(session_id.to_string())
        .execute(transaction.connection())
        .await
        .map_err(|e| database_error(&e))?;
    if done.rows_affected() == 0 { Err(PlatformError::Refused) } else { Ok(()) }
}

/// End one session (logout).
pub async fn revoke_session(transaction: &mut PlatformTransaction<'_>, session_id: Uuid) -> Result<(), PlatformError> {
    sqlx::query("UPDATE platform_sessions SET revoked_at = now(), revoke_reason = 'logout' WHERE id = $1::uuid AND revoked_at IS NULL")
        .bind(session_id.to_string())
        .execute(transaction.connection())
        .await
        .map_err(|e| database_error(&e))?;
    Ok(())
}

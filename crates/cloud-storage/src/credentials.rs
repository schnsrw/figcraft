//! Local accounts (S6-032): the tenant-scoped credentials record of a principal.
//!
//! A principal stays the identity; this table only says "this principal can sign in to this tenant
//! with this login and password". The password hash is an opaque PHC string produced by
//! `figcraft_cloud_auth::password` (it embeds algorithm and parameters, so parameters can be raised
//! later and old hashes upgraded at the next successful login); this module never sees a password.
//! Every function runs in a tenant transaction, so forced row-level security keeps one tenant's
//! logins invisible to another and `ada` may exist in two tenants.
//!
//! Lockout (S6-035) lives here because the counters are shared by every replica: a failed attempt
//! against an *unlocked* account counts, and from [`LockoutPolicy::threshold`] failures the account
//! is locked for `base * 2^(failures - threshold)` seconds up to `max`. Attempts made while locked
//! are refused without counting, so an attacker cannot keep a victim locked out forever by hammering
//! the account. A success clears the counters. A locked and an unknown account are indistinguishable
//! to the client (the route returns the same fixed error after the same work).

use figcraft_cloud_domain::PrincipalId;
use std::fmt;
use unicode_normalization::UnicodeNormalization;

use crate::TenantTransaction;
use crate::tenants::audit_success;

/// Shortest login accepted, in characters.
pub const MIN_LOGIN_CHARS: usize = 3;
/// Longest login accepted, in characters.
pub const MAX_LOGIN_CHARS: usize = 128;
/// Longest stored password hash (PHC string), in bytes.
pub const MAX_HASH_BYTES: usize = 512;

/// Fixed, non-leaking categories for credential failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialError {
    /// The login is empty, too short or long, or holds whitespace or control characters.
    InvalidLogin,
    /// The password hash is empty, oversized or not a plausible PHC string.
    InvalidHash,
    /// The lockout policy is out of range.
    InvalidPolicy,
    /// The login is taken in this tenant, or the principal already has a credential here.
    Conflict,
    /// The principal does not exist.
    UnknownPrincipal,
    /// No credential matches (for updates that name a principal).
    NotFound,
    Audit,
    Database,
    CorruptRow,
}

impl fmt::Display for CredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "credential error: {self:?}")
    }
}

impl std::error::Error for CredentialError {}

fn database_error(error: &sqlx::Error) -> CredentialError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23505") => CredentialError::Conflict,
        Some("23503") => CredentialError::UnknownPrincipal,
        Some("23514") => CredentialError::InvalidLogin,
        _ => CredentialError::Database,
    }
}

/// The stored form of a login: trimmed, Unicode NFC, lowercased (then NFC again, since lowercasing
/// can denormalise), 3 to 128 characters, no whitespace or control characters. `None` when it does
/// not qualify. Visually confusable characters are *not* folded (that needs a skeleton table);
/// logins are not secrets and a same-looking pair is two distinct accounts.
#[must_use]
pub fn normalize_login(login: &str) -> Option<String> {
    let trimmed = login.trim();
    // Bound the work before normalising: no valid login is longer than this in bytes.
    if trimmed.is_empty() || trimmed.len() > MAX_LOGIN_CHARS * 8 {
        return None;
    }
    let normalized: String = trimmed.nfc().collect::<String>().to_lowercase().nfc().collect();
    let chars = normalized.chars().count();
    if !(MIN_LOGIN_CHARS..=MAX_LOGIN_CHARS).contains(&chars) || normalized.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    Some(normalized)
}

/// Whether the account may sign in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialState {
    Active,
    Disabled,
}

impl CredentialState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Disabled => "disabled",
        }
    }

    fn parse(value: &str) -> Result<Self, CredentialError> {
        match value {
            "active" => Ok(Self::Active),
            "disabled" => Ok(Self::Disabled),
            _ => Err(CredentialError::CorruptRow),
        }
    }
}

/// When and for how long a failing account is locked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LockoutPolicy {
    /// Failures (on an unlocked account) before the first lock; 1 to 100.
    pub threshold: u32,
    /// The first lock's length in seconds; 1 to `max_secs`.
    pub base_secs: i64,
    /// The longest lock in seconds; at most one day.
    pub max_secs: i64,
}

impl Default for LockoutPolicy {
    /// Five failures, then 30 s, doubling per further failure, never past 15 minutes.
    fn default() -> Self {
        Self { threshold: 5, base_secs: 30, max_secs: 15 * 60 }
    }
}

impl LockoutPolicy {
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.threshold >= 1 && self.threshold <= 100 && self.base_secs >= 1 && self.base_secs <= self.max_secs && self.max_secs <= 24 * 60 * 60
    }
}

/// A credential as the sign-in path reads it. Never printed with its hash.
#[derive(Clone, Eq, PartialEq)]
pub struct CredentialRecord {
    pub principal_id: PrincipalId,
    pub login: String,
    pub password_hash: String,
    pub state: CredentialState,
    pub failed_attempts: u32,
    /// Seconds until the lock ends; zero when not locked.
    pub locked_for_secs: i64,
}

impl fmt::Debug for CredentialRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialRecord")
            .field("principal_id", &self.principal_id)
            .field("login", &self.login)
            .field("password_hash", &"[REDACTED]")
            .field("state", &self.state)
            .field("failed_attempts", &self.failed_attempts)
            .field("locked_for_secs", &self.locked_for_secs)
            .finish()
    }
}

type Row = (String, String, String, String, i32, i64);

const BY_LOGIN_SQL: &str = concat!(
    "SELECT principal_id::text, login, password_hash, state, failed_attempts, ",
    "GREATEST(0, ceil(extract(epoch FROM COALESCE(locked_until, now()) - now())))::bigint FROM local_credentials ",
    "WHERE tenant_id = $1::uuid AND login = $2"
);

const BY_PRINCIPAL_SQL: &str = concat!(
    "SELECT principal_id::text, login, password_hash, state, failed_attempts, ",
    "GREATEST(0, ceil(extract(epoch FROM COALESCE(locked_until, now()) - now())))::bigint FROM local_credentials ",
    "WHERE tenant_id = $1::uuid AND principal_id = $2::uuid"
);

fn parse(row: Row) -> Result<CredentialRecord, CredentialError> {
    let (principal, login, password_hash, state, failed, locked) = row;
    Ok(CredentialRecord {
        principal_id: principal.parse().map_err(|_| CredentialError::CorruptRow)?,
        login,
        password_hash,
        state: CredentialState::parse(&state)?,
        failed_attempts: u32::try_from(failed).map_err(|_| CredentialError::CorruptRow)?,
        locked_for_secs: locked.max(0),
    })
}

fn plausible_hash(hash: &str) -> bool {
    hash.starts_with('$') && hash.len() >= 32 && hash.len() <= MAX_HASH_BYTES && hash.bytes().all(|b| b.is_ascii_graphic())
}

/// Give `principal_id` a credential in the transaction's tenant. `login` is normalised here
/// ([`normalize_login`]); `password_hash` is a PHC string. A taken login, or a principal that
/// already has a credential in this tenant, is [`CredentialError::Conflict`]. Records
/// `credential.create` (the login itself is not copied into the audit trail). The caller commits.
pub async fn create_credential(
    transaction: &mut TenantTransaction<'_>,
    principal_id: PrincipalId,
    login: &str,
    password_hash: &str,
    actor: Option<PrincipalId>,
) -> Result<(), CredentialError> {
    let login = normalize_login(login).ok_or(CredentialError::InvalidLogin)?;
    if !plausible_hash(password_hash) {
        return Err(CredentialError::InvalidHash);
    }
    sqlx::query("INSERT INTO local_credentials (tenant_id, principal_id, login, password_hash) VALUES ($1::uuid, $2::uuid, $3, $4)")
        .bind(transaction.tenant_id().to_string())
        .bind(principal_id.to_string())
        .bind(&login)
        .bind(password_hash)
        .execute(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    audit_success(transaction, actor, "credential.create", "principal", Some(principal_id.as_uuid()), &serde_json::json!({}))
        .await
        .map_err(|_| CredentialError::Audit)?;
    Ok(())
}

/// The credential with `login` (normalised here) in the transaction's tenant.
pub async fn find_by_login(transaction: &mut TenantTransaction<'_>, login: &str) -> Result<Option<CredentialRecord>, CredentialError> {
    let Some(login) = normalize_login(login) else {
        return Ok(None);
    };
    let row: Option<Row> = sqlx::query_as(BY_LOGIN_SQL)
        .bind(transaction.tenant_id().to_string())
        .bind(&login)
        .fetch_optional(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    row.map(parse).transpose()
}

/// The credential of `principal_id` in the transaction's tenant.
pub async fn find_for_principal(
    transaction: &mut TenantTransaction<'_>,
    principal_id: PrincipalId,
) -> Result<Option<CredentialRecord>, CredentialError> {
    let row: Option<Row> = sqlx::query_as(BY_PRINCIPAL_SQL)
        .bind(transaction.tenant_id().to_string())
        .bind(principal_id.to_string())
        .fetch_optional(transaction.connection())
        .await
        .map_err(|error| database_error(&error))?;
    row.map(parse).transpose()
}

/// Count a failed sign-in against `principal_id` and lock the account per `policy` when the
/// threshold is reached. Does nothing (and returns the remaining lock) when the account is already
/// locked, so hammering a locked account never extends the lock. Returns the seconds of lock now
/// in force (zero when none).
pub async fn record_failure(
    transaction: &mut TenantTransaction<'_>,
    principal_id: PrincipalId,
    policy: LockoutPolicy,
) -> Result<i64, CredentialError> {
    if !policy.is_valid() {
        return Err(CredentialError::InvalidPolicy);
    }
    let locked: Option<i64> = sqlx::query_scalar(concat!(
        "UPDATE local_credentials SET failed_attempts = failed_attempts + 1, last_failed_at = clock_timestamp(), ",
        "updated_at = clock_timestamp(), locked_until = CASE WHEN failed_attempts + 1 >= $3::int THEN ",
        "clock_timestamp() + make_interval(secs => LEAST($5::bigint, ($4::bigint << LEAST(failed_attempts + 1 - $3::int, 20)))) ",
        "ELSE locked_until END ",
        "WHERE tenant_id = $1::uuid AND principal_id = $2::uuid AND (locked_until IS NULL OR locked_until <= clock_timestamp()) ",
        "RETURNING GREATEST(0, ceil(extract(epoch FROM COALESCE(locked_until, now()) - clock_timestamp())))::bigint"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(principal_id.to_string())
    .bind(i32::try_from(policy.threshold).map_err(|_| CredentialError::InvalidPolicy)?)
    .bind(policy.base_secs)
    .bind(policy.max_secs)
    .fetch_optional(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    if let Some(secs) = locked {
        return Ok(secs.max(0));
    }
    // Already locked (or no such credential): report the lock in force.
    let remaining: Option<i64> = sqlx::query_scalar(concat!(
        "SELECT GREATEST(0, ceil(extract(epoch FROM COALESCE(locked_until, now()) - clock_timestamp())))::bigint ",
        "FROM local_credentials WHERE tenant_id = $1::uuid AND principal_id = $2::uuid"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(principal_id.to_string())
    .fetch_optional(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    remaining.map(|secs| secs.max(0)).ok_or(CredentialError::NotFound)
}

/// Clear the failure counters and any lock after a successful sign-in.
pub async fn record_success(transaction: &mut TenantTransaction<'_>, principal_id: PrincipalId) -> Result<(), CredentialError> {
    sqlx::query(concat!(
        "UPDATE local_credentials SET failed_attempts = 0, last_failed_at = NULL, locked_until = NULL ",
        "WHERE tenant_id = $1::uuid AND principal_id = $2::uuid AND (failed_attempts <> 0 OR locked_until IS NOT NULL)"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(principal_id.to_string())
    .execute(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    Ok(())
}

/// Why a password hash is being replaced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PasswordChange {
    /// The user changed it (needs the current password; audited as `credential.password_change`).
    Change,
    /// A reset link was used (audited as `credential.password_reset`).
    Reset,
}

/// Replace the password hash of `principal_id` (a new password): clears the counters and any lock
/// and stamps `password_changed_at`. Audited.
pub async fn set_password(
    transaction: &mut TenantTransaction<'_>,
    principal_id: PrincipalId,
    password_hash: &str,
    kind: PasswordChange,
    actor: Option<PrincipalId>,
) -> Result<(), CredentialError> {
    if !plausible_hash(password_hash) {
        return Err(CredentialError::InvalidHash);
    }
    let updated = sqlx::query(concat!(
        "UPDATE local_credentials SET password_hash = $3, password_changed_at = clock_timestamp(), updated_at = clock_timestamp(), ",
        "failed_attempts = 0, last_failed_at = NULL, locked_until = NULL WHERE tenant_id = $1::uuid AND principal_id = $2::uuid"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(principal_id.to_string())
    .bind(password_hash)
    .execute(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    if updated.rows_affected() != 1 {
        return Err(CredentialError::NotFound);
    }
    let action = match kind {
        PasswordChange::Change => "credential.password_change",
        PasswordChange::Reset => "credential.password_reset",
    };
    audit_success(transaction, actor, action, "principal", Some(principal_id.as_uuid()), &serde_json::json!({}))
        .await
        .map_err(|_| CredentialError::Audit)
}

/// Store a stronger hash of the *same* password (rehash on login, S6-033): only when the stored
/// hash is still `old_hash`, so a concurrent password change is never overwritten. Not audited and
/// does not touch `password_changed_at`. True when it was replaced.
pub async fn upgrade_hash(
    transaction: &mut TenantTransaction<'_>,
    principal_id: PrincipalId,
    old_hash: &str,
    new_hash: &str,
) -> Result<bool, CredentialError> {
    if !plausible_hash(new_hash) {
        return Err(CredentialError::InvalidHash);
    }
    let updated = sqlx::query(concat!(
        "UPDATE local_credentials SET password_hash = $4, updated_at = clock_timestamp() ",
        "WHERE tenant_id = $1::uuid AND principal_id = $2::uuid AND password_hash = $3"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(principal_id.to_string())
    .bind(old_hash)
    .bind(new_hash)
    .execute(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    Ok(updated.rows_affected() == 1)
}

/// Enable or disable sign-in for `principal_id`. Returns true when the state changed. Audited as
/// `credential.enable` or `credential.disable`. Ending the account's live sessions is the
/// caller's job (it also touches membership and the session table).
pub async fn set_state(
    transaction: &mut TenantTransaction<'_>,
    principal_id: PrincipalId,
    state: CredentialState,
    actor: PrincipalId,
) -> Result<bool, CredentialError> {
    let updated = sqlx::query(concat!(
        "UPDATE local_credentials SET state = $3, updated_at = clock_timestamp() ",
        "WHERE tenant_id = $1::uuid AND principal_id = $2::uuid AND state <> $3"
    ))
    .bind(transaction.tenant_id().to_string())
    .bind(principal_id.to_string())
    .bind(state.as_str())
    .execute(transaction.connection())
    .await
    .map_err(|error| database_error(&error))?;
    if updated.rows_affected() != 1 {
        return Ok(false);
    }
    let action = match state {
        CredentialState::Active => "credential.enable",
        CredentialState::Disabled => "credential.disable",
    };
    audit_success(transaction, Some(actor), action, "principal", Some(principal_id.as_uuid()), &serde_json::json!({}))
        .await
        .map_err(|_| CredentialError::Audit)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logins_are_normalised() {
        assert_eq!(normalize_login("  Ada@Example.COM ").as_deref(), Some("ada@example.com"));
        // NFD and NFC spellings of one letter are one login; case folds.
        assert_eq!(normalize_login("Jose\u{301}"), normalize_login("JOS\u{c9}"));
        assert_eq!(normalize_login("Jose\u{301}").as_deref(), Some("jos\u{e9}"));
    }

    #[test]
    fn bad_logins_are_refused() {
        let long = "x".repeat(129);
        let huge = "x".repeat(100_000);
        for bad in ["", "  ", "ab", "a b c", "ada\nx", "ada\u{0}", "ada\u{7f}x", long.as_str(), huge.as_str()] {
            assert_eq!(normalize_login(bad), None, "{bad:?}");
        }
        assert!(normalize_login(&"x".repeat(128)).is_some());
    }

    #[test]
    fn lockout_policy_is_bounded() {
        assert!(LockoutPolicy::default().is_valid());
        for bad in [
            LockoutPolicy { threshold: 0, ..LockoutPolicy::default() },
            LockoutPolicy { threshold: 101, ..LockoutPolicy::default() },
            LockoutPolicy { base_secs: 0, ..LockoutPolicy::default() },
            LockoutPolicy { base_secs: 1000, max_secs: 10, ..LockoutPolicy::default() },
            LockoutPolicy { max_secs: 24 * 60 * 60 + 1, ..LockoutPolicy::default() },
        ] {
            assert!(!bad.is_valid(), "{bad:?}");
        }
    }

    #[test]
    fn records_never_print_their_hash() {
        let record = CredentialRecord {
            principal_id: PrincipalId::random(),
            login: "ada".into(),
            password_hash: "$argon2id$secret".into(),
            state: CredentialState::Active,
            failed_attempts: 0,
            locked_for_secs: 0,
        };
        assert!(!format!("{record:?}").contains("argon2id"));
    }

    #[test]
    fn hash_plausibility() {
        assert!(plausible_hash(&format!("${}", "a".repeat(40))));
        assert!(!plausible_hash("plain"));
        assert!(!plausible_hash(&format!("${}", "a".repeat(600))));
        assert!(!plausible_hash(&format!("${} x", "a".repeat(40))));
    }
}

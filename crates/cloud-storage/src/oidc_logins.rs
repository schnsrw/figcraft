//! OIDC login attempts and identity-link resolution (S4-001, storage half).
//!
//! A login attempt is written when the authorization-code flow starts: the SHA-256 of the random
//! `state` (never the state itself), the configured issuer, the nonce, the PKCE verifier and an
//! optional relative `return_to`, with an expiry. The callback consumes it with one conditional
//! `UPDATE … RETURNING`, so a state is usable once, only before it expires and only for the issuer
//! that started it; every other case is the same [`OidcLoginError::Refused`].
//!
//! [`resolve_identity`] maps a verified `(issuer, subject)` to a principal: an existing link
//! refreshes its bounded `verified_metadata`; a first login creates a `user` principal and its link
//! in one transaction (a concurrent first login for the same identity loses the insert race and
//! resolves to the winner's principal). Both run before any tenant is known, through the pool,
//! like [`resolve_share_link`](crate::share_links::resolve_share_link).

use figcraft_cloud_domain::PrincipalId;
use sqlx::PgPool;

/// Longest issuer URL accepted (the column bound).
pub const MAX_ISSUER_CHARS: usize = 2048;
/// Longest subject accepted (the column bound).
pub const MAX_SUBJECT_CHARS: usize = 1024;
/// Longest `return_to` stored (the column bound).
pub const MAX_RETURN_TO_CHARS: usize = 2048;
/// Longest display name for a new principal (the column bound).
pub const MAX_DISPLAY_NAME_CHARS: usize = 255;
/// Largest serialized `verified_metadata` stored per identity link.
pub const MAX_VERIFIED_METADATA_BYTES: usize = 4096;
/// Longest login-attempt lifetime accepted (the flow uses ten minutes).
pub const MAX_ATTEMPT_LIFETIME_SECS: u64 = 60 * 60;
/// Most expired attempts removed by one [`delete_expired_login_attempts`] call.
pub const MAX_CLEANUP_BATCH: i64 = 10_000;

/// Fixed, non-leaking categories for login-attempt and identity failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OidcLoginError {
    /// An argument is outside the stored bounds.
    InvalidInput,
    /// The state does not open a login (unknown, expired, already used or another issuer).
    /// Deliberately one category.
    Refused,
    /// The identity resolves to a principal that is suspended, deleted or pending.
    PrincipalInactive,
    /// A stored row violates a constraint (for example a duplicate state hash).
    Constraint,
    Database,
}

impl std::fmt::Display for OidcLoginError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "oidc login error: {self:?}")
    }
}

impl std::error::Error for OidcLoginError {}

/// A new login attempt, as written by the start of the flow.
pub struct NewLoginAttempt<'a> {
    /// SHA-256 of the state sent to the provider.
    pub state_hash: [u8; 32],
    pub issuer: &'a str,
    pub nonce: &'a str,
    pub pkce_verifier: &'a str,
    pub return_to: Option<&'a str>,
    pub lifetime_secs: u64,
}

impl std::fmt::Debug for NewLoginAttempt<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NewLoginAttempt")
            .field("issuer", &self.issuer)
            .field("nonce", &"[REDACTED]")
            .field("pkce_verifier", &"[REDACTED]")
            .field("return_to", &self.return_to)
            .field("lifetime_secs", &self.lifetime_secs)
            .finish_non_exhaustive()
    }
}

/// What a consumed attempt hands back to the callback.
#[derive(Clone, Eq, PartialEq)]
pub struct ConsumedLoginAttempt {
    pub nonce: String,
    pub pkce_verifier: String,
    pub return_to: Option<String>,
}

impl std::fmt::Debug for ConsumedLoginAttempt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConsumedLoginAttempt")
            .field("nonce", &"[REDACTED]")
            .field("pkce_verifier", &"[REDACTED]")
            .field("return_to", &self.return_to)
            .finish()
    }
}

/// The principal a verified identity maps to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedIdentity {
    pub principal_id: PrincipalId,
    /// True when this login created the principal and its identity link.
    pub new_principal: bool,
}

fn bounded(value: &str, min: usize, max: usize) -> bool {
    let chars = value.chars().count();
    chars >= min && chars <= max
}

fn database_error(error: &sqlx::Error) -> OidcLoginError {
    match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23505" | "23503" | "23514") => OidcLoginError::Constraint,
        _ => OidcLoginError::Database,
    }
}

/// Record a started login. The caller has generated the state, nonce and verifier; only the
/// state's hash is stored.
pub async fn insert_login_attempt(pool: &PgPool, attempt: &NewLoginAttempt<'_>) -> Result<(), OidcLoginError> {
    if !bounded(attempt.issuer, 1, MAX_ISSUER_CHARS)
        || !bounded(attempt.nonce, 16, 256)
        || !bounded(attempt.pkce_verifier, 43, 128)
        || attempt.return_to.is_some_and(|value| !bounded(value, 1, MAX_RETURN_TO_CHARS))
        || attempt.lifetime_secs == 0
        || attempt.lifetime_secs > MAX_ATTEMPT_LIFETIME_SECS
    {
        return Err(OidcLoginError::InvalidInput);
    }
    let lifetime = i64::try_from(attempt.lifetime_secs).map_err(|_| OidcLoginError::InvalidInput)?;
    sqlx::query(
        "INSERT INTO oidc_login_attempts (state_hash, issuer, nonce, pkce_verifier, return_to, expires_at) \
         VALUES ($1, $2, $3, $4, $5, now() + make_interval(secs => $6::bigint))",
    )
    .bind(attempt.state_hash.to_vec())
    .bind(attempt.issuer)
    .bind(attempt.nonce)
    .bind(attempt.pkce_verifier)
    .bind(attempt.return_to)
    .bind(lifetime)
    .execute(pool)
    .await
    .map_err(|error| database_error(&error))?;
    Ok(())
}

/// Consume the attempt whose state hashes to `state_hash` for `issuer`: exactly once, and only
/// before it expires. Anything else is [`OidcLoginError::Refused`].
pub async fn consume_login_attempt(pool: &PgPool, state_hash: &[u8; 32], issuer: &str) -> Result<ConsumedLoginAttempt, OidcLoginError> {
    if !bounded(issuer, 1, MAX_ISSUER_CHARS) {
        return Err(OidcLoginError::Refused);
    }
    let row: Option<(String, String, Option<String>)> = sqlx::query_as(
        "UPDATE oidc_login_attempts SET consumed_at = now() \
         WHERE state_hash = $1 AND issuer = $2 AND consumed_at IS NULL AND expires_at > now() \
         RETURNING nonce, pkce_verifier, return_to",
    )
    .bind(state_hash.to_vec())
    .bind(issuer)
    .fetch_optional(pool)
    .await
    .map_err(|_| OidcLoginError::Database)?;
    let (nonce, pkce_verifier, return_to) = row.ok_or(OidcLoginError::Refused)?;
    Ok(ConsumedLoginAttempt { nonce, pkce_verifier, return_to })
}

/// Remove up to `limit` (at most [`MAX_CLEANUP_BATCH`]) attempts that expired or were consumed
/// more than `grace_secs` ago; returns how many were removed.
pub async fn delete_expired_login_attempts(pool: &PgPool, grace_secs: u32, limit: i64) -> Result<u64, OidcLoginError> {
    if !(1..=MAX_CLEANUP_BATCH).contains(&limit) {
        return Err(OidcLoginError::InvalidInput);
    }
    let result = sqlx::query(
        "DELETE FROM oidc_login_attempts WHERE state_hash IN ( \
           SELECT state_hash FROM oidc_login_attempts \
           WHERE expires_at < now() - make_interval(secs => $1::bigint) \
              OR consumed_at < now() - make_interval(secs => $1::bigint) \
           LIMIT $2)",
    )
    .bind(i64::from(grace_secs))
    .bind(limit)
    .execute(pool)
    .await
    .map_err(|_| OidcLoginError::Database)?;
    Ok(result.rows_affected())
}

/// Map a verified `(issuer, subject)` to its principal, creating a `user` principal named
/// `display_name` and the identity link on first login. `verified_metadata` must be a JSON object
/// of at most [`MAX_VERIFIED_METADATA_BYTES`] serialized bytes; it replaces the stored metadata.
/// A principal that is not `active` is [`OidcLoginError::PrincipalInactive`].
pub async fn resolve_identity(
    pool: &PgPool,
    issuer: &str,
    subject: &str,
    display_name: &str,
    verified_metadata: &serde_json::Value,
) -> Result<ResolvedIdentity, OidcLoginError> {
    let display_name = display_name.trim();
    if !bounded(issuer, 1, MAX_ISSUER_CHARS) || !bounded(subject, 1, MAX_SUBJECT_CHARS) || !bounded(display_name, 1, MAX_DISPLAY_NAME_CHARS) {
        return Err(OidcLoginError::InvalidInput);
    }
    if !verified_metadata.is_object() {
        return Err(OidcLoginError::InvalidInput);
    }
    let metadata = serde_json::to_string(verified_metadata).map_err(|_| OidcLoginError::InvalidInput)?;
    if metadata.len() > MAX_VERIFIED_METADATA_BYTES {
        return Err(OidcLoginError::InvalidInput);
    }

    // Existing link: refresh the verified claims and return its principal.
    let existing: Option<(String, String)> = sqlx::query_as(
        "WITH link AS ( \
           UPDATE identity_links SET verified_metadata = $3::jsonb, updated_at = now() \
           WHERE issuer = $1 AND subject = $2 RETURNING principal_id) \
         SELECT p.id::text, p.state FROM link JOIN principals p ON p.id = link.principal_id",
    )
    .bind(issuer)
    .bind(subject)
    .bind(&metadata)
    .fetch_optional(pool)
    .await
    .map_err(|error| database_error(&error))?;
    if let Some((principal_id, state)) = existing {
        return existing_principal(&principal_id, &state);
    }

    // First login: create the principal and its link together.
    let principal_id = PrincipalId::random();
    let mut transaction = pool.begin().await.map_err(|_| OidcLoginError::Database)?;
    sqlx::query("INSERT INTO principals (id, kind, display_name, state) VALUES ($1::uuid, 'user', $2, 'active')")
        .bind(principal_id.to_string())
        .bind(display_name)
        .execute(&mut *transaction)
        .await
        .map_err(|error| database_error(&error))?;
    let inserted: Option<(String,)> = sqlx::query_as(
        "INSERT INTO identity_links (principal_id, issuer, subject, verified_metadata) VALUES ($1::uuid, $2, $3, $4::jsonb) \
         ON CONFLICT (issuer, subject) DO NOTHING RETURNING principal_id::text",
    )
    .bind(principal_id.to_string())
    .bind(issuer)
    .bind(subject)
    .bind(&metadata)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|error| database_error(&error))?;
    if inserted.is_some() {
        transaction.commit().await.map_err(|_| OidcLoginError::Database)?;
        return Ok(ResolvedIdentity { principal_id, new_principal: true });
    }
    // Lost a race with a concurrent first login: drop our principal and use the winner's.
    transaction.rollback().await.map_err(|_| OidcLoginError::Database)?;
    let winner: Option<(String, String)> = sqlx::query_as(
        "SELECT p.id::text, p.state FROM identity_links l JOIN principals p ON p.id = l.principal_id \
         WHERE l.issuer = $1 AND l.subject = $2",
    )
    .bind(issuer)
    .bind(subject)
    .fetch_optional(pool)
    .await
    .map_err(|_| OidcLoginError::Database)?;
    let (principal_id, state) = winner.ok_or(OidcLoginError::Database)?;
    existing_principal(&principal_id, &state)
}

fn existing_principal(principal_id: &str, state: &str) -> Result<ResolvedIdentity, OidcLoginError> {
    if state != "active" {
        return Err(OidcLoginError::PrincipalInactive);
    }
    Ok(ResolvedIdentity { principal_id: principal_id.parse().map_err(|_| OidcLoginError::Database)?, new_principal: false })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempts_never_print_their_secrets() {
        let attempt = NewLoginAttempt {
            state_hash: [0; 32],
            issuer: "https://issuer.example",
            nonce: "nonce-nonce-nonce-nonce",
            pkce_verifier: "verifier",
            return_to: Some("/files"),
            lifetime_secs: 600,
        };
        let printed = format!("{attempt:?}");
        assert!(!printed.contains("nonce-nonce") && !printed.contains("verifier\""));
        let consumed = ConsumedLoginAttempt { nonce: "n-secret".into(), pkce_verifier: "v-secret".into(), return_to: None };
        let printed = format!("{consumed:?}");
        assert!(!printed.contains("n-secret") && !printed.contains("v-secret"));
    }

    #[test]
    fn bounds_count_characters() {
        assert!(bounded("é", 1, 1));
        assert!(!bounded("", 1, 2));
        assert!(!bounded("abc", 1, 2));
    }
}

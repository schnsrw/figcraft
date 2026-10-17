//! One-use embed-token identifiers (S6-003).
//!
//! `consume_embed_jti` records a host's token `jti`; the primary key makes the first use win and
//! every later use (concurrent or not) fail, atomically in PostgreSQL. Rows are kept until the
//! token could no longer validate (expiry plus skew) and are swept in small bounded batches.

use std::fmt;

use crate::TenantTransaction;

pub const MIN_JTI_BYTES: usize = 8;
pub const MAX_JTI_BYTES: usize = 128;
/// Expired rows removed opportunistically by each consume, and by one explicit sweep.
const OPPORTUNISTIC_SWEEP: i64 = 20;
pub const MAX_SWEEP_BATCH: i64 = 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmbedTokenStoreError {
    InvalidInput,
    /// The host is not registered in this tenant.
    NotFound,
    Database,
}

impl fmt::Display for EmbedTokenStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "embed token store error: {self:?}")
    }
}

impl std::error::Error for EmbedTokenStoreError {}

/// Whether `jti` is an acceptable identifier: 8 to 128 bytes of `[A-Za-z0-9._~-]`.
#[must_use]
pub fn valid_jti(jti: &str) -> bool {
    (MIN_JTI_BYTES..=MAX_JTI_BYTES).contains(&jti.len())
        && jti.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'~' | b'-'))
}

/// Consume a token identifier. Returns `true` for the first use and `false` for any replay. The
/// row stays until `expires_at_unix` (the token's last possible validity, skew included).
pub async fn consume_embed_jti(
    transaction: &mut TenantTransaction<'_>,
    system_id: &str,
    jti: &str,
    expires_at_unix: i64,
) -> Result<bool, EmbedTokenStoreError> {
    if !valid_jti(jti) || expires_at_unix < 0 {
        return Err(EmbedTokenStoreError::InvalidInput);
    }
    sweep_embed_jtis(transaction, OPPORTUNISTIC_SWEEP).await?;
    let result = sqlx::query(
        "INSERT INTO embed_token_uses (tenant_id, system_id, jti, expires_at) \
         VALUES ($1::uuid, $2, $3, to_timestamp($4::bigint)) ON CONFLICT DO NOTHING",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(system_id)
    .bind(jti)
    .bind(expires_at_unix)
    .execute(transaction.connection())
    .await
    .map_err(|error| match error.as_database_error().and_then(|database| database.code()).as_deref() {
        Some("23503") => EmbedTokenStoreError::NotFound,
        Some("23514") => EmbedTokenStoreError::InvalidInput,
        _ => EmbedTokenStoreError::Database,
    })?;
    Ok(result.rows_affected() == 1)
}

/// Delete up to `limit` (capped at [`MAX_SWEEP_BATCH`]) of this tenant's identifiers whose tokens
/// can no longer validate, by the database clock. Returns how many were removed.
pub async fn sweep_embed_jtis(transaction: &mut TenantTransaction<'_>, limit: i64) -> Result<u64, EmbedTokenStoreError> {
    let limit = limit.clamp(1, MAX_SWEEP_BATCH);
    let result = sqlx::query(
        "DELETE FROM embed_token_uses WHERE ctid IN ( \
             SELECT ctid FROM embed_token_uses WHERE tenant_id = $1::uuid AND expires_at < clock_timestamp() LIMIT $2)",
    )
    .bind(transaction.tenant_id().to_string())
    .bind(limit)
    .execute(transaction.connection())
    .await
    .map_err(|_| EmbedTokenStoreError::Database)?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::valid_jti;

    #[test]
    fn jti_is_bounded_url_safe_text() {
        assert!(valid_jti("abcdefgh"));
        assert!(valid_jti("0a1b2c3d-4e5f-6789-abcd-ef0123456789"));
        assert!(valid_jti(&"a".repeat(128)));
        assert!(!valid_jti("short"));
        assert!(!valid_jti(&"a".repeat(129)));
        assert!(!valid_jti("has space here"));
        assert!(!valid_jti("quote'quote"));
        assert!(!valid_jti("new\nline12"));
    }
}

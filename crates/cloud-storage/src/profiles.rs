//! A person's profile (S6-036): display name and colour, and their own browser sessions.
//!
//! The profile belongs to the principal, not to a tenant (a person is one person in every workspace
//! they belong to), so these functions run on the pool. Display names are 1 to 64 characters; the
//! colour is one of the [`PALETTE`] and, when never chosen, derives from the principal id so every
//! person has a stable colour from the start ([`color_for`]).
//!
//! Session management lists the person's *live, non-embedded* browser sessions (embedded sessions
//! belong to a host system and are not the person's to manage; never the token), revokes one
//! (their own only) or all but the current one.

use figcraft_cloud_domain::{PrincipalId, Uuid};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::fmt;

use crate::accounts::normalize_display_name;

/// The colours a person may pick (and the set defaults are drawn from): twelve mid-tone hues that
/// stay legible on light and dark canvases.
pub const PALETTE: [&str; 12] =
    ["#e5484d", "#f76b15", "#e0a100", "#46a758", "#12a594", "#0090ff", "#3e63dd", "#8e4ec6", "#d6409f", "#ab6400", "#6e56cf", "#5b6b7c"];

/// Most live sessions listed.
pub const MAX_SESSIONS_LISTED: i64 = 100;

/// Fixed, non-leaking categories for profile failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProfileError {
    InvalidName,
    InvalidColor,
    NotFound,
    Database,
    CorruptRow,
}

impl fmt::Display for ProfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "profile error: {self:?}")
    }
}

impl std::error::Error for ProfileError {}

/// The stable default colour of a principal: a palette entry chosen by hashing the id.
#[must_use]
pub fn color_for(principal_id: PrincipalId) -> &'static str {
    let digest = Sha256::digest(principal_id.as_uuid().as_bytes());
    let index = usize::from(digest[0]) % PALETTE.len();
    PALETTE.get(index).copied().unwrap_or(PALETTE[0])
}

/// A person's profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Profile {
    pub principal_id: PrincipalId,
    pub display_name: String,
    /// The colour in effect: the chosen one, or the derived default.
    pub color: String,
    /// Whether the colour was chosen (false: derived).
    pub color_chosen: bool,
}

/// The profile of `principal_id`.
pub async fn get_profile(pool: &PgPool, principal_id: PrincipalId) -> Result<Profile, ProfileError> {
    let row: Option<(String, Option<String>)> = sqlx::query_as("SELECT display_name, color FROM principals WHERE id = $1::uuid")
        .bind(principal_id.to_string())
        .fetch_optional(pool)
        .await
        .map_err(|_| ProfileError::Database)?;
    let (display_name, color) = row.ok_or(ProfileError::NotFound)?;
    Ok(Profile { principal_id, display_name, color_chosen: color.is_some(), color: color.unwrap_or_else(|| color_for(principal_id).to_string()) })
}

/// Change the display name and/or colour (at least one). `color` must be in the [`PALETTE`].
/// Returns the new profile.
pub async fn update_profile(
    pool: &PgPool,
    principal_id: PrincipalId,
    display_name: Option<&str>,
    color: Option<&str>,
) -> Result<Profile, ProfileError> {
    let name = match display_name {
        Some(name) => Some(normalize_display_name(name).ok_or(ProfileError::InvalidName)?),
        None => None,
    };
    if let Some(color) = color
        && !PALETTE.contains(&color)
    {
        return Err(ProfileError::InvalidColor);
    }
    let updated = sqlx::query(concat!(
        "UPDATE principals SET display_name = COALESCE($2, display_name), color = COALESCE($3, color), updated_at = now() ",
        "WHERE id = $1::uuid AND kind = 'user' AND state = 'active'"
    ))
    .bind(principal_id.to_string())
    .bind(name)
    .bind(color)
    .execute(pool)
    .await
    .map_err(|_| ProfileError::Database)?;
    if updated.rows_affected() != 1 {
        return Err(ProfileError::NotFound);
    }
    get_profile(pool, principal_id).await
}

/// One live browser session of a person (never its token).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionSummary {
    pub id: Uuid,
    /// Unix seconds.
    pub created_at: i64,
    pub last_seen_at: i64,
    pub idle_expires_at: i64,
    pub absolute_expires_at: i64,
}

/// The person's live browser sessions, newest first, at most [`MAX_SESSIONS_LISTED`].
pub async fn list_sessions(pool: &PgPool, principal_id: PrincipalId) -> Result<Vec<SessionSummary>, ProfileError> {
    let rows: Vec<(String, i64, i64, i64, i64)> = sqlx::query_as(concat!(
        "SELECT id::text, extract(epoch FROM created_at)::bigint, extract(epoch FROM last_seen_at)::bigint, ",
        "extract(epoch FROM idle_expires_at)::bigint, extract(epoch FROM absolute_expires_at)::bigint ",
        "FROM browser_sessions WHERE principal_id = $1::uuid AND revoked_at IS NULL ",
        "AND idle_expires_at > now() AND absolute_expires_at > now() AND embed_system_id IS NULL ORDER BY created_at DESC, id LIMIT $2"
    ))
    .bind(principal_id.to_string())
    .bind(MAX_SESSIONS_LISTED)
    .fetch_all(pool)
    .await
    .map_err(|_| ProfileError::Database)?;
    rows.into_iter()
        .map(|(id, created_at, last_seen_at, idle_expires_at, absolute_expires_at)| {
            Ok(SessionSummary {
                id: Uuid::parse_str(&id).map_err(|_| ProfileError::CorruptRow)?,
                created_at,
                last_seen_at,
                idle_expires_at,
                absolute_expires_at,
            })
        })
        .collect()
}

/// Revoke session `session_id` if it is `principal_id`'s and still live. True when it was revoked.
pub async fn revoke_session(pool: &PgPool, principal_id: PrincipalId, session_id: Uuid) -> Result<bool, ProfileError> {
    let result = sqlx::query(concat!(
        "UPDATE browser_sessions SET revoked_at = now(), revoke_reason = 'logout' ",
        "WHERE id = $1::uuid AND principal_id = $2::uuid AND revoked_at IS NULL"
    ))
    .bind(session_id.to_string())
    .bind(principal_id.to_string())
    .execute(pool)
    .await
    .map_err(|_| ProfileError::Database)?;
    Ok(result.rows_affected() == 1)
}

/// Revoke every live session of `principal_id` except `keep`. Returns how many.
pub async fn revoke_other_sessions(pool: &PgPool, principal_id: PrincipalId, keep: Uuid) -> Result<u64, ProfileError> {
    let result = sqlx::query(concat!(
        "UPDATE browser_sessions SET revoked_at = now(), revoke_reason = 'logout' ",
        "WHERE principal_id = $1::uuid AND id <> $2::uuid AND revoked_at IS NULL"
    ))
    .bind(principal_id.to_string())
    .bind(keep.to_string())
    .execute(pool)
    .await
    .map_err(|_| ProfileError::Database)?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_colours_are_stable_palette_entries() {
        let id = PrincipalId::random();
        assert_eq!(color_for(id), color_for(id));
        assert!(PALETTE.contains(&color_for(id)));
        let distinct: std::collections::BTreeSet<_> = (0..200).map(|_| color_for(PrincipalId::random())).collect();
        assert!(distinct.len() >= 8, "defaults spread over the palette: {}", distinct.len());
    }

    #[test]
    fn the_palette_is_valid_lowercase_hex() {
        for color in PALETTE {
            assert!(
                color.len() == 7 && color.starts_with('#') && color[1..].bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "{color}"
            );
        }
        let unique: std::collections::BTreeSet<_> = PALETTE.iter().collect();
        assert_eq!(unique.len(), PALETTE.len());
    }
}

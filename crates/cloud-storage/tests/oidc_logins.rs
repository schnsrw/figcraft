//! OIDC login attempts and identity links against the dev PostgreSQL (S4-001; skipped without
//! FIGCRAFT_TEST_DATABASE_URL_FILE). Every run uses a fresh issuer and random state hashes and
//! subjects, so reruns never collide.

use figcraft_cloud_domain::Uuid;
use figcraft_cloud_storage::oidc_logins::{
    MAX_VERIFIED_METADATA_BYTES, NewLoginAttempt, OidcLoginError, consume_login_attempt, delete_expired_login_attempts, insert_login_attempt,
    resolve_identity,
};
use serde_json::json;
use sqlx::postgres::{PgPool, PgPoolOptions};

type TestResult = Result<(), Box<dyn std::error::Error>>;

async fn pool() -> Result<Option<PgPool>, Box<dyn std::error::Error>> {
    let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; OIDC login probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    Ok(Some(PgPoolOptions::new().max_connections(4).connect(database_url.trim_end()).await?))
}

fn issuer() -> String {
    format!("https://a2-{}.issuer.test", Uuid::new_v4())
}

fn hash() -> [u8; 32] {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    bytes
}

const NONCE: &str = "a2-nonce-0123456789abcdef";
const VERIFIER: &str = "a2-verifier-0123456789abcdefghijklmnopqrstuvwxyz";

fn attempt<'a>(state_hash: [u8; 32], issuer: &'a str, return_to: Option<&'a str>) -> NewLoginAttempt<'a> {
    NewLoginAttempt { state_hash, issuer, nonce: NONCE, pkce_verifier: VERIFIER, return_to, lifetime_secs: 600 }
}

#[tokio::test]
async fn attempts_are_consumed_once_for_their_issuer_only() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let issuer = issuer();
    let state = hash();
    insert_login_attempt(&pool, &attempt(state, &issuer, Some("/files"))).await?;
    // The same state cannot be started twice.
    assert_eq!(insert_login_attempt(&pool, &attempt(state, &issuer, None)).await, Err(OidcLoginError::Constraint));
    // Another issuer cannot consume it.
    assert_eq!(consume_login_attempt(&pool, &state, "https://other.issuer.test").await, Err(OidcLoginError::Refused));
    let consumed = consume_login_attempt(&pool, &state, &issuer).await?;
    assert_eq!((consumed.nonce.as_str(), consumed.pkce_verifier.as_str(), consumed.return_to.as_deref()), (NONCE, VERIFIER, Some("/files")));
    assert_eq!(consume_login_attempt(&pool, &state, &issuer).await, Err(OidcLoginError::Refused));
    assert_eq!(consume_login_attempt(&pool, &hash(), &issuer).await, Err(OidcLoginError::Refused));
    Ok(())
}

#[tokio::test]
async fn expired_attempts_are_refused_and_cleaned_up() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let issuer = issuer();
    let state = hash();
    insert_login_attempt(&pool, &attempt(state, &issuer, None)).await?;
    sqlx::query(
        "UPDATE oidc_login_attempts SET created_at = now() - interval '2 hours', expires_at = now() - interval '1 hour' WHERE state_hash = $1",
    )
    .bind(state.to_vec())
    .execute(&pool)
    .await?;
    assert_eq!(consume_login_attempt(&pool, &state, &issuer).await, Err(OidcLoginError::Refused));
    let removed = delete_expired_login_attempts(&pool, 60, 10_000).await?;
    assert!(removed >= 1);
    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM oidc_login_attempts WHERE state_hash = $1").bind(state.to_vec()).fetch_one(&pool).await?;
    assert_eq!(left, 0);
    assert_eq!(delete_expired_login_attempts(&pool, 60, 0).await, Err(OidcLoginError::InvalidInput));
    Ok(())
}

#[tokio::test]
async fn attempt_inputs_are_bounded() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let issuer = issuer();
    let long_path = format!("/{}", "a".repeat(2048));
    for bad in [
        NewLoginAttempt { nonce: "short", ..attempt(hash(), &issuer, None) },
        NewLoginAttempt { pkce_verifier: "short", ..attempt(hash(), &issuer, None) },
        NewLoginAttempt { lifetime_secs: 0, ..attempt(hash(), &issuer, None) },
        NewLoginAttempt { lifetime_secs: 24 * 3600, ..attempt(hash(), &issuer, None) },
        NewLoginAttempt { issuer: "", ..attempt(hash(), &issuer, None) },
        attempt(hash(), &issuer, Some(&long_path)),
    ] {
        assert_eq!(insert_login_attempt(&pool, &bad).await, Err(OidcLoginError::InvalidInput));
    }
    Ok(())
}

#[tokio::test]
async fn identities_map_to_one_principal_and_refresh_their_claims() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let issuer = issuer();
    let subject = format!("a2-{}", Uuid::new_v4());
    let first = resolve_identity(&pool, &issuer, &subject, "  A2 Person  ", &json!({"email": "a@example.test"})).await?;
    assert!(first.new_principal);
    let second = resolve_identity(&pool, &issuer, &subject, "Renamed", &json!({"email": "b@example.test"})).await?;
    assert_eq!(second.principal_id, first.principal_id);
    assert!(!second.new_principal);
    let (name, metadata): (String, serde_json::Value) = sqlx::query_as(
        "SELECT p.display_name, l.verified_metadata FROM identity_links l JOIN principals p ON p.id = l.principal_id \
         WHERE l.issuer = $1 AND l.subject = $2",
    )
    .bind(&issuer)
    .bind(&subject)
    .fetch_one(&pool)
    .await?;
    // The display name is the principal's own once created; only the verified claims refresh.
    assert_eq!(name, "A2 Person");
    assert_eq!(metadata, json!({"email": "b@example.test"}));

    // The same subject at another issuer is a different identity.
    let elsewhere = resolve_identity(&pool, &self::issuer(), &subject, "A2 Person", &json!({})).await?;
    assert!(elsewhere.new_principal);
    assert_ne!(elsewhere.principal_id, first.principal_id);

    // Suspended principals cannot log in.
    sqlx::query("UPDATE principals SET state = 'suspended' WHERE id = $1::uuid").bind(first.principal_id.to_string()).execute(&pool).await?;
    assert_eq!(resolve_identity(&pool, &issuer, &subject, "A2 Person", &json!({})).await, Err(OidcLoginError::PrincipalInactive));
    Ok(())
}

#[tokio::test]
async fn identity_inputs_are_bounded() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let issuer = issuer();
    let huge = json!({"name": "x".repeat(MAX_VERIFIED_METADATA_BYTES)});
    assert_eq!(resolve_identity(&pool, &issuer, "a2-s", "n", &huge).await, Err(OidcLoginError::InvalidInput));
    assert_eq!(resolve_identity(&pool, &issuer, "a2-s", "n", &json!(["not", "an", "object"])).await, Err(OidcLoginError::InvalidInput));
    assert_eq!(resolve_identity(&pool, &issuer, "", "n", &json!({})).await, Err(OidcLoginError::InvalidInput));
    assert_eq!(resolve_identity(&pool, &issuer, &"s".repeat(1025), "n", &json!({})).await, Err(OidcLoginError::InvalidInput));
    assert_eq!(resolve_identity(&pool, &issuer, "a2-s", "   ", &json!({})).await, Err(OidcLoginError::InvalidInput));
    Ok(())
}

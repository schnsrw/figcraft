//! Browser sessions against the dev PostgreSQL (fixture prefix c3; skipped without
//! FIGCRAFT_TEST_DATABASE_URL_FILE). Every test uses fresh principals, so reruns pass.

use figcraft_cloud_domain::PrincipalId;
use figcraft_cloud_storage::sessions::{
    RevokeReason, SessionError, SessionLimits, TOKEN_CHARS, create_session, logout, resolve_session, revoke_all_for_principal,
    revoke_expired_sessions, rotate_session, session_token_hash,
};
use sqlx::postgres::{PgPool, PgPoolOptions};

type TestResult = Result<(), Box<dyn std::error::Error>>;

async fn pool() -> Result<Option<PgPool>, Box<dyn std::error::Error>> {
    let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; session probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    Ok(Some(PgPoolOptions::new().max_connections(4).connect(database_url.trim_end()).await?))
}

async fn principal(pool: &PgPool, state: &str) -> Result<PrincipalId, Box<dyn std::error::Error>> {
    let suffix = uuid_suffix();
    let id: PrincipalId = format!("c3500000-0000-4000-8000-{suffix}").parse()?;
    sqlx::query("INSERT INTO principals (id, kind, display_name, state) VALUES ($1::uuid, 'user', 'c3 session probe', $2)")
        .bind(id.to_string())
        .bind(state)
        .execute(pool)
        .await?;
    Ok(id)
}

fn uuid_suffix() -> String {
    let simple = figcraft_cloud_domain::Uuid::new_v4().simple().to_string();
    simple.chars().skip(20).collect()
}

async fn set_principal_state(pool: &PgPool, id: PrincipalId, state: &str) -> TestResult {
    sqlx::query("UPDATE principals SET state = $2 WHERE id = $1::uuid").bind(id.to_string()).bind(state).execute(pool).await?;
    Ok(())
}

async fn row(pool: &PgPool, token: &str) -> Result<(String, Option<String>, Option<String>, f64), Box<dyn std::error::Error>> {
    Ok(sqlx::query_as(
        "SELECT id::text, rotated_from::text, revoke_reason, extract(epoch FROM idle_expires_at - now())::float8 FROM browser_sessions WHERE token_hash = $1",
    )
    .bind(session_token_hash(token).to_vec())
    .fetch_one(pool)
    .await?)
}

#[tokio::test]
async fn sessions_resolve_to_their_principal_and_store_only_a_hash() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let limits = SessionLimits::default();
    let user = principal(&pool, "active").await?;
    let created = create_session(&pool, user, limits).await?;
    assert_eq!(created.token.len(), TOKEN_CHARS);
    assert!(!format!("{created:?}").contains(&created.token));
    let resolved = resolve_session(&pool, &created.token, limits).await?;
    assert_eq!(resolved.id, created.id);
    assert_eq!(resolved.principal_id, user);
    assert!(resolved.age_secs < 60);
    assert!(resolved.absolute_remaining_secs > limits.absolute_secs - 60);
    let leaked: i64 = sqlx::query_scalar("SELECT count(*) FROM browser_sessions WHERE encode(token_hash, 'escape') LIKE '%' || $1 || '%'")
        .bind(&created.token)
        .fetch_one(&pool)
        .await?;
    assert_eq!(leaked, 0);

    // Pending or unknown principals cannot get a session.
    let pending = principal(&pool, "pending").await?;
    assert_eq!(create_session(&pool, pending, limits).await, Err(SessionError::InactivePrincipal));
    assert_eq!(create_session(&pool, PrincipalId::random(), limits).await, Err(SessionError::InactivePrincipal));
    let bad = SessionLimits { idle_secs: 0, ..limits };
    assert_eq!(create_session(&pool, user, bad).await, Err(SessionError::InvalidInput));
    Ok(())
}

#[tokio::test]
async fn unknown_malformed_revoked_and_expired_sessions_are_refused_alike() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let limits = SessionLimits::default();
    let user = principal(&pool, "active").await?;
    let created = create_session(&pool, user, limits).await?;
    let mut wrong = created.token.clone().into_bytes();
    if let Some(first) = wrong.first_mut() {
        *first = if *first == b'A' { b'B' } else { b'A' };
    }
    let wrong = String::from_utf8(wrong)?;
    for bad in [wrong.as_str(), "", "short", &"x".repeat(4096), &format!("{} ", created.token)] {
        assert_eq!(resolve_session(&pool, bad, limits).await, Err(SessionError::Refused), "{bad:?}");
    }

    // Idle expiry (backdated).
    let idle = create_session(&pool, user, limits).await?;
    sqlx::query("UPDATE browser_sessions SET created_at = now() - interval '9 hours', last_seen_at = now() - interval '9 hours', idle_expires_at = now() - interval '1 hour' WHERE id = $1::uuid")
        .bind(idle.id.to_string())
        .execute(&pool)
        .await?;
    assert_eq!(resolve_session(&pool, &idle.token, limits).await, Err(SessionError::Refused));

    // Absolute expiry wins even with a live idle expiry.
    let absolute = create_session(&pool, user, limits).await?;
    sqlx::query(
        "UPDATE browser_sessions SET created_at = now() - interval '8 days', absolute_expires_at = now() - interval '1 second' WHERE id = $1::uuid",
    )
    .bind(absolute.id.to_string())
    .execute(&pool)
    .await?;
    assert_eq!(resolve_session(&pool, &absolute.token, limits).await, Err(SessionError::Refused));
    assert_eq!(rotate_session(&pool, &absolute.token, limits).await, Err(SessionError::Refused));

    // Housekeeping marks both expired sessions revoked with reason 'expired'.
    assert!(revoke_expired_sessions(&pool, 10_000).await? >= 2);
    assert_eq!(row(&pool, &idle.token).await?.2.as_deref(), Some("expired"));
    assert_eq!(row(&pool, &absolute.token).await?.2.as_deref(), Some("expired"));

    // Suspended principal: refused while suspended, live again when reactivated.
    set_principal_state(&pool, user, "suspended").await?;
    assert_eq!(resolve_session(&pool, &created.token, limits).await, Err(SessionError::Refused));
    set_principal_state(&pool, user, "active").await?;
    assert!(resolve_session(&pool, &created.token, limits).await.is_ok());

    // Logout revokes; second logout and junk are idempotent no-ops.
    assert!(logout(&pool, &created.token).await?);
    assert!(!logout(&pool, &created.token).await?);
    assert!(!logout(&pool, "junk").await?);
    assert_eq!(row(&pool, &created.token).await?.2.as_deref(), Some("logout"));
    assert_eq!(resolve_session(&pool, &created.token, limits).await, Err(SessionError::Refused));
    Ok(())
}

#[tokio::test]
async fn idle_expiry_slides_with_bounded_writes_and_never_past_absolute() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let limits = SessionLimits::default();
    let user = principal(&pool, "active").await?;
    let created = create_session(&pool, user, limits).await?;

    // Within the touch interval: no write.
    sqlx::query("UPDATE browser_sessions SET idle_expires_at = now() + interval '1 hour' WHERE id = $1::uuid")
        .bind(created.id.to_string())
        .execute(&pool)
        .await?;
    resolve_session(&pool, &created.token, limits).await?;
    let (_, _, _, remaining) = row(&pool, &created.token).await?;
    assert!(remaining < 3700.0, "touched too often: {remaining}");

    // Past the touch interval: slid back to the full idle timeout.
    sqlx::query("UPDATE browser_sessions SET last_seen_at = now() - interval '2 minutes' WHERE id = $1::uuid")
        .bind(created.id.to_string())
        .execute(&pool)
        .await?;
    resolve_session(&pool, &created.token, limits).await?;
    let (_, _, _, remaining) = row(&pool, &created.token).await?;
    assert!(remaining > (limits.idle_secs - 60) as f64, "not slid: {remaining}");

    // Close to the absolute expiry: capped by it.
    sqlx::query("UPDATE browser_sessions SET last_seen_at = now() - interval '2 minutes', absolute_expires_at = now() + interval '30 minutes' WHERE id = $1::uuid")
        .bind(created.id.to_string())
        .execute(&pool)
        .await?;
    let resolved = resolve_session(&pool, &created.token, limits).await?;
    assert!(resolved.absolute_remaining_secs <= 1800);
    let (_, _, _, remaining) = row(&pool, &created.token).await?;
    assert!(remaining <= 1800.5, "slid past absolute: {remaining}");
    Ok(())
}

#[tokio::test]
async fn rotation_revokes_the_old_session_and_links_the_new_one() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let limits = SessionLimits::default();
    let user = principal(&pool, "active").await?;
    let first = create_session(&pool, user, limits).await?;
    sqlx::query("UPDATE browser_sessions SET absolute_expires_at = now() + interval '2 days' WHERE id = $1::uuid")
        .bind(first.id.to_string())
        .execute(&pool)
        .await?;
    let second = rotate_session(&pool, &first.token, limits).await?;
    let third = rotate_session(&pool, &second.token, limits).await?;
    assert_ne!(second.token, first.token);
    assert!(second.absolute_remaining_secs <= 2 * 24 * 3600, "rotation extended the lifetime");

    let (first_id, first_from, first_reason, _) = row(&pool, &first.token).await?;
    let (second_id, second_from, second_reason, _) = row(&pool, &second.token).await?;
    let (_, third_from, third_reason, _) = row(&pool, &third.token).await?;
    assert_eq!((first_from, first_reason.as_deref()), (None, Some("rotated")));
    assert_eq!((second_from.as_deref(), second_reason.as_deref()), (Some(first_id.as_str()), Some("rotated")));
    assert_eq!((third_from.as_deref(), third_reason), (Some(second_id.as_str()), None));

    assert_eq!(resolve_session(&pool, &first.token, limits).await, Err(SessionError::Refused));
    assert_eq!(resolve_session(&pool, &second.token, limits).await, Err(SessionError::Refused));
    assert_eq!(resolve_session(&pool, &third.token, limits).await?.principal_id, user);
    // A rotated-out token cannot be rotated again (replay).
    assert_eq!(rotate_session(&pool, &first.token, limits).await, Err(SessionError::Refused));

    // Concurrent rotations of one token: exactly one wins.
    let (a, b) = tokio::join!(rotate_session(&pool, &third.token, limits), rotate_session(&pool, &third.token, limits));
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1, "{a:?} {b:?}");
    Ok(())
}

#[tokio::test]
async fn revoke_all_for_principal_ends_every_session_of_that_principal_only() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let limits = SessionLimits::default();
    let (user, other) = (principal(&pool, "active").await?, principal(&pool, "active").await?);
    let sessions =
        [create_session(&pool, user, limits).await?, create_session(&pool, user, limits).await?, create_session(&pool, user, limits).await?];
    let kept = create_session(&pool, other, limits).await?;
    assert_eq!(revoke_all_for_principal(&pool, user, RevokeReason::MembershipRevoked).await?, 3);
    for session in &sessions {
        assert_eq!(resolve_session(&pool, &session.token, limits).await, Err(SessionError::Refused));
        assert_eq!(row(&pool, &session.token).await?.2.as_deref(), Some("membership_revoked"));
    }
    assert_eq!(revoke_all_for_principal(&pool, user, RevokeReason::Operator).await?, 0);
    assert_eq!(resolve_session(&pool, &kept.token, limits).await?.principal_id, other);
    Ok(())
}

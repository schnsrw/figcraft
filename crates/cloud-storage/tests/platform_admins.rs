//! S6-045 platform admins, sessions and bootstrap against PostgreSQL; set
//! FIGCRAFT_TEST_DATABASE_URL_FILE to a migrated runtime URL.
//!
//! The platform tables are global, so tests that need an empty table (bootstrap, last-admin
//! protection) empty it inside a transaction they roll back; everything else uses unique logins
//! and removes what it made.

use figcraft_cloud_domain::Uuid;
use figcraft_cloud_storage::platform::{PlatformError, begin_platform_transaction};
use figcraft_cloud_storage::platform_admins::{
    AdminState, PlatformSessionLimits, accept_totp_step, bootstrap_first_admin, confirm_totp, create_admin, create_session, credentials_by_login,
    get_admin, issue_bootstrap_token, mark_second_factor_done, normalize_login, record_failed_login, record_successful_login, resolve_session,
    revoke_admin_sessions, revoke_session, set_admin_password, set_admin_state, store_totp_secret,
};
use sqlx::postgres::{PgPool, PgPoolOptions};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// The platform tables are global, so these tests take turns.
static TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const HASH: &str = "$argon2id$v=19$m=8192,t=1,p=1$c29tZXNhbHQ$aGFzaGhhc2hoYXNoaGFzaGhhc2g";

async fn pool() -> TestResult<Option<PgPool>> {
    let Ok(file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; platform admin probe skipped");
        return Ok(None);
    };
    let url = std::fs::read_to_string(file)?;
    Ok(Some(PgPoolOptions::new().max_connections(4).connect(url.trim_end()).await?))
}

fn login(prefix: &str) -> String {
    format!("{prefix}-{}@ops.example", &Uuid::new_v4().simple().to_string()[..12])
}

async fn remove(pool: &PgPool, id: Uuid) -> TestResult {
    let mut tx = begin_platform_transaction(pool).await?;
    sqlx::query("DELETE FROM platform_admins WHERE id = $1::uuid").bind(id.to_string()).execute(tx.connection()).await?;
    tx.commit().await?;
    Ok(())
}

#[test]
fn logins_are_normalized_and_bounded() {
    assert_eq!(normalize_login("  Ada@Ops.Example ").as_deref(), Some("ada@ops.example"));
    for bad in ["", "ab", "has space@x.y", "semi;colon", "ünï@x.y", &"a".repeat(255)] {
        assert_eq!(normalize_login(bad), None, "{bad:?}");
    }
}

#[tokio::test]
async fn the_first_admin_is_created_once_with_the_one_time_token() -> TestResult {
    let _turn = TURN.lock().await;
    let Some(pool) = pool().await? else { return Ok(()) };
    let mut tx = begin_platform_transaction(&pool).await?;
    // Start from an empty realm inside a transaction that is never committed.
    sqlx::query("DELETE FROM platform_admins").execute(tx.connection()).await?;
    sqlx::query("DELETE FROM platform_bootstrap").execute(tx.connection()).await?;

    assert_eq!(issue_bootstrap_token(&mut tx, 10).await, Err(PlatformError::Invalid), "lifetimes are bounded");
    let token = issue_bootstrap_token(&mut tx, 600).await?;
    assert_eq!(token.len(), 43);
    let wrong = "A".repeat(43);
    assert_eq!(bootstrap_first_admin(&mut tx, &wrong, "root@ops.example", "Root", HASH).await.err(), Some(PlatformError::Refused));
    assert_eq!(bootstrap_first_admin(&mut tx, "short", "root@ops.example", "Root", HASH).await.err(), Some(PlatformError::Refused));
    assert_eq!(bootstrap_first_admin(&mut tx, &token, "x", "Root", HASH).await.err(), Some(PlatformError::Invalid));
    let admin = bootstrap_first_admin(&mut tx, &token, "Root@Ops.Example", "Root", HASH).await?;
    assert_eq!(admin.login, "root@ops.example");
    assert!(admin.has_password && !admin.totp_confirmed && admin.state == AdminState::Active);
    // Spent: the same token, and a new one, are refused once an admin exists.
    assert_eq!(bootstrap_first_admin(&mut tx, &token, "second@ops.example", "Second", HASH).await.err(), Some(PlatformError::Refused));
    assert_eq!(issue_bootstrap_token(&mut tx, 600).await, Err(PlatformError::Refused));
    tx.rollback().await?;

    // An expired token is refused.
    let mut tx = begin_platform_transaction(&pool).await?;
    sqlx::query("DELETE FROM platform_admins").execute(tx.connection()).await?;
    let token = issue_bootstrap_token(&mut tx, 600).await?;
    sqlx::query("UPDATE platform_bootstrap SET expires_at = now() - interval '1 second'").execute(tx.connection()).await?;
    assert_eq!(bootstrap_first_admin(&mut tx, &token, "late@ops.example", "Late", HASH).await.err(), Some(PlatformError::Refused));
    tx.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn the_last_active_admin_cannot_be_disabled() -> TestResult {
    let _turn = TURN.lock().await;
    let Some(pool) = pool().await? else { return Ok(()) };
    let mut tx = begin_platform_transaction(&pool).await?;
    sqlx::query("DELETE FROM platform_admins").execute(tx.connection()).await?;
    let first = create_admin(&mut tx, None, &login("one"), "One", HASH).await?;
    let second = create_admin(&mut tx, Some(first.id), &login("two"), "Two", HASH).await?;
    assert_eq!(second.state, AdminState::Active);
    assert_eq!(set_admin_state(&mut tx, second.id, AdminState::Disabled).await?.state, AdminState::Disabled);
    assert_eq!(set_admin_state(&mut tx, first.id, AdminState::Disabled).await.err(), Some(PlatformError::Refused));
    assert_eq!(set_admin_state(&mut tx, second.id, AdminState::Active).await?.state, AdminState::Active);
    assert_eq!(set_admin_state(&mut tx, first.id, AdminState::Disabled).await?.state, AdminState::Disabled);
    assert_eq!(set_admin_state(&mut tx, Uuid::new_v4(), AdminState::Disabled).await.err(), Some(PlatformError::NotFound));
    tx.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn sessions_need_the_second_factor_and_end_with_the_admin() -> TestResult {
    let _turn = TURN.lock().await;
    let Some(pool) = pool().await? else { return Ok(()) };
    let mut tx = begin_platform_transaction(&pool).await?;
    let admin = create_admin(&mut tx, None, &login("sess"), "Sess", HASH).await?;
    let limits = PlatformSessionLimits::default();
    let created = create_session(&mut tx, admin.id, false, limits).await?;
    assert_eq!(created.token.len(), 43);
    tx.commit().await?;

    let mut tx = begin_platform_transaction(&pool).await?;
    let resolved = resolve_session(&mut tx, &created.token, limits).await?;
    assert_eq!((resolved.admin_id, resolved.second_factor_done), (admin.id, false));
    mark_second_factor_done(&mut tx, resolved.id).await?;
    assert!(resolve_session(&mut tx, &created.token, limits).await?.second_factor_done);
    // Anything that is not a live token is the same refusal.
    for bad in ["", "short", &"A".repeat(43), &"!".repeat(43)] {
        assert_eq!(resolve_session(&mut tx, bad, limits).await.err(), Some(PlatformError::Refused), "{bad:?}");
    }
    assert_eq!(resolve_session(&mut tx, &created.token, PlatformSessionLimits { idle_secs: 0, ..limits }).await.err(), Some(PlatformError::Invalid));
    // Idle expiry.
    sqlx::query("UPDATE platform_sessions SET idle_expires_at = now() - interval '1 second' WHERE id = $1::uuid")
        .bind(created.id.to_string())
        .execute(tx.connection())
        .await?;
    assert_eq!(resolve_session(&mut tx, &created.token, limits).await.err(), Some(PlatformError::Refused));
    // Logout and disabling end live sessions.
    let second = create_session(&mut tx, admin.id, true, limits).await?;
    revoke_session(&mut tx, second.id).await?;
    assert_eq!(resolve_session(&mut tx, &second.token, limits).await.err(), Some(PlatformError::Refused));
    let third = create_session(&mut tx, admin.id, true, limits).await?;
    assert_eq!(revoke_admin_sessions(&mut tx, admin.id, "operator").await?, 2, "the idle-expired session was never revoked, so it counts too");
    assert_eq!(resolve_session(&mut tx, &third.token, limits).await.err(), Some(PlatformError::Refused));
    assert_eq!(revoke_admin_sessions(&mut tx, admin.id, "nonsense").await.err(), Some(PlatformError::Invalid));
    tx.commit().await?;

    // A disabled admin cannot open or keep a session; a password change revokes sessions.
    let mut tx = begin_platform_transaction(&pool).await?;
    let other = create_admin(&mut tx, None, &login("sess2"), "Sess2", HASH).await?;
    let live = create_session(&mut tx, other.id, true, limits).await?;
    set_admin_password(&mut tx, other.id, HASH).await?;
    assert_eq!(resolve_session(&mut tx, &live.token, limits).await.err(), Some(PlatformError::Refused));
    let again = create_session(&mut tx, other.id, true, limits).await?;
    set_admin_state(&mut tx, other.id, AdminState::Disabled).await?;
    assert_eq!(resolve_session(&mut tx, &again.token, limits).await.err(), Some(PlatformError::Refused));
    assert_eq!(create_session(&mut tx, other.id, false, limits).await.err(), Some(PlatformError::Refused));
    tx.commit().await?;
    remove(&pool, admin.id).await?;
    remove(&pool, other.id).await?;
    Ok(())
}

#[tokio::test]
async fn failed_sign_ins_lock_and_totp_steps_are_single_use() -> TestResult {
    let _turn = TURN.lock().await;
    let Some(pool) = pool().await? else { return Ok(()) };
    let mut tx = begin_platform_transaction(&pool).await?;
    let admin = create_admin(&mut tx, None, &login("lock"), "Lock", HASH).await?;
    assert_eq!(credentials_by_login(&mut tx, &admin.login).await?.map(|c| (c.failed_logins, c.locked_for_secs)), Some((0, 0)));
    assert!(credentials_by_login(&mut tx, "nobody@ops.example").await?.is_none());
    assert!(credentials_by_login(&mut tx, "bad login").await?.is_none());
    for expected in 1..=2 {
        assert_eq!(record_failed_login(&mut tx, admin.id, 3, 900).await?, expected);
    }
    assert_eq!(credentials_by_login(&mut tx, &admin.login).await?.map(|c| c.locked_for_secs), Some(0));
    assert_eq!(record_failed_login(&mut tx, admin.id, 3, 900).await?, 3);
    let locked = credentials_by_login(&mut tx, &admin.login).await?.ok_or("admin")?;
    assert!(locked.locked_for_secs > 800 && locked.locked_for_secs <= 900);
    record_successful_login(&mut tx, admin.id).await?;
    let cleared = credentials_by_login(&mut tx, &admin.login).await?.ok_or("admin")?;
    assert_eq!((cleared.failed_logins, cleared.locked_for_secs), (0, 0));
    assert!(get_admin(&mut tx, admin.id).await?.ok_or("admin")?.last_login_at.is_some());

    // The secret is stored sealed (opaque bytes) and counts only after confirmation; a step is
    // accepted once and never an older one.
    store_totp_secret(&mut tx, admin.id, &[7; 40]).await?;
    assert!(!get_admin(&mut tx, admin.id).await?.ok_or("admin")?.totp_confirmed);
    confirm_totp(&mut tx, admin.id).await?;
    assert!(get_admin(&mut tx, admin.id).await?.ok_or("admin")?.totp_confirmed);
    assert!(accept_totp_step(&mut tx, admin.id, 1000).await?);
    assert!(!accept_totp_step(&mut tx, admin.id, 1000).await?, "the same step twice");
    assert!(!accept_totp_step(&mut tx, admin.id, 999).await?, "an older step");
    assert!(accept_totp_step(&mut tx, admin.id, 1001).await?);
    assert_eq!(confirm_totp(&mut tx, Uuid::new_v4()).await.err(), Some(PlatformError::NotFound));
    tx.commit().await?;
    remove(&pool, admin.id).await?;
    Ok(())
}

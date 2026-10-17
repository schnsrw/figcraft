//! S4-023: revocation racing use, against the dev PostgreSQL (fixture prefix d6; skipped without
//! FIGCRAFT_TEST_DATABASE_URL_FILE).
//!
//! Each race is between real connections, every racer on a pool of its own with exactly one
//! connection (so its backend pid is known), on a multi-thread runtime. Where both sides take a
//! row lock, a gate transaction holds that row first and the test waits, through
//! `pg_stat_activity`, until each racer is blocked on it, in a chosen arrival order; PostgreSQL
//! grants a contended row lock to waiters in arrival order, so each ordering is forced and both
//! orderings are tested. Where the use side is a plain read (share-link exchange, credential
//! authentication), there is nothing to block on, so the test holds the revoking transaction open
//! instead and shows that the revoke's commit is the linearisation point: a use that starts after
//! the commit returns is always refused, and a use that read the row before it is visible to the
//! relay's revocation check (`dead_share_links`) at once, so the sweeper closes it within one
//! interval. No sleep stands in for synchronisation.
//!
//! The documented outcomes:
//! * share link: exchange before the revoke commits succeeds; every exchange starting after the
//!   commit is refused, and every grant handed out earlier is reported dead immediately.
//! * session: a resolve that has read the session before a logout/revoke-all commits succeeds,
//!   but its sliding-expiry touch never undoes the revoke; afterwards the token is refused.
//! * session rotation vs revoke-all: whichever wins, no live session is left for the principal.
//! * service-account credential: as for share links (a stale last_used touch blocked on the
//!   revoke completes without undoing it).
//! * invitation: accept and revoke serialise on the invitation row; exactly one wins and the
//!   loser changes nothing.

use std::time::{Duration, Instant};

use figcraft_cloud_domain::{PrincipalId, ProjectId, TenantId, Uuid, WorkspaceId};
use figcraft_cloud_storage::invitations::{InvitationError, accept_invitation, create_invitation, revoke_invitation};
use figcraft_cloud_storage::metadata::{create_document, create_project, create_workspace};
use figcraft_cloud_storage::service_accounts::{
    Scope, ServiceAccountError, authenticate_credential, create_service_account, issue_credential, revoke_credential,
};
use figcraft_cloud_storage::sessions::{
    RevokeReason, SessionError, SessionLimits, create_session, logout, resolve_session, revoke_all_for_principal, rotate_session,
};
use figcraft_cloud_storage::share_links::{ShareLinkError, ShareRole, create_share_link, dead_share_links, exchange_share_token, revoke_share_link};
use figcraft_cloud_storage::teams::{TeamRole, create_team};
use figcraft_cloud_storage::tenants::{NewTenant, create_tenant_with_owner};
use figcraft_cloud_storage::{TenantTransaction, begin_tenant_transaction};
use sqlx::postgres::{PgPool, PgPoolOptions};
use tokio::task::JoinHandle;
use tokio::time::timeout;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type TestResult<T = ()> = Result<T, BoxError>;

/// Upper bound on any single wait in these tests.
const BOUND: Duration = Duration::from_secs(20);

fn d6() -> Uuid {
    let mut bytes = *Uuid::new_v4().as_bytes();
    bytes[0] = 0xd6;
    Uuid::from_bytes(bytes)
}

fn suffix() -> String {
    d6().simple().to_string()[2..14].to_owned()
}

fn database_url() -> TestResult<Option<String>> {
    let Ok(file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; revocation race probe skipped");
        return Ok(None);
    };
    Ok(Some(std::fs::read_to_string(file)?.trim_end().to_string()))
}

async fn shared_pool(url: &str) -> TestResult<PgPool> {
    Ok(PgPoolOptions::new().max_connections(4).acquire_timeout(BOUND).connect(url).await?)
}

/// A racer's own single-connection pool and that connection's backend pid.
async fn racer_pool(url: &str) -> TestResult<(PgPool, i32)> {
    let pool = PgPoolOptions::new().max_connections(1).min_connections(1).acquire_timeout(BOUND).connect(url).await?;
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&pool).await?;
    Ok((pool, pid))
}

async fn tx(pool: &PgPool, tenant: TenantId) -> TestResult<TenantTransaction<'_>> {
    begin_tenant_transaction(pool, tenant).await.map_err(|error| format!("begin tenant: {error:?}").into())
}

/// Wait until every pid in `pids` is waiting on a lock.
async fn wait_until_blocked(pool: &PgPool, pids: &[i32]) -> TestResult {
    let expected = i64::try_from(pids.len())?;
    let deadline = Instant::now() + BOUND;
    loop {
        let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE pid = ANY($1) AND wait_event_type = 'Lock'")
            .bind(pids)
            .fetch_one(pool)
            .await?;
        if waiting == expected {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(format!("only {waiting} of {expected} racers blocked on the lock").into());
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn join<T>(handle: JoinHandle<TestResult<T>>) -> TestResult<T> {
    timeout(BOUND, handle).await??
}

async fn principal(pool: &PgPool) -> TestResult<PrincipalId> {
    let id = d6();
    sqlx::query("INSERT INTO principals (id, kind, display_name) VALUES ($1::uuid, 'user', 'd6 revocation race probe')")
        .bind(id.to_string())
        .execute(pool)
        .await?;
    Ok(PrincipalId::try_from(id).map_err(|_| "nil principal")?)
}

/// A fresh active tenant owned by a fresh principal, with a workspace and project.
struct Tenant {
    id: TenantId,
    owner: PrincipalId,
    workspace: WorkspaceId,
    project: ProjectId,
}

async fn tenant(pool: &PgPool) -> TestResult<Tenant> {
    let owner = principal(pool).await?;
    let id = TenantId::try_from(d6()).map_err(|_| "nil tenant")?;
    let slug = format!("d6-{}", suffix());
    let mut t = tx(pool, id).await?;
    create_tenant_with_owner(&mut t, NewTenant { slug: &slug, name: "d6 revocation races", plan_id: "team", home_region: "local" }, owner)
        .await
        .map_err(|error| format!("{error:?}"))?;
    let workspace = WorkspaceId::try_from(d6()).map_err(|_| "nil")?;
    create_workspace(&mut t, workspace, "d6 races", None).await.map_err(|error| format!("{error:?}"))?;
    let project = ProjectId::try_from(d6()).map_err(|_| "nil")?;
    create_project(&mut t, project, workspace, "d6 races", None).await.map_err(|error| format!("{error:?}"))?;
    t.commit().await?;
    Ok(Tenant { id, owner, workspace, project })
}

// ---------------------------------------------------------------------------------------------
// Share links
// ---------------------------------------------------------------------------------------------

async fn share_link(pool: &PgPool, tenant: &Tenant) -> TestResult<(Uuid, String)> {
    let mut t = tx(pool, tenant.id).await?;
    let document = figcraft_cloud_domain::CloudDocumentId::random();
    create_document(&mut t, document, tenant.workspace, tenant.project, "d6 race doc").await.map_err(|error| format!("{error:?}"))?;
    let link = create_share_link(&mut t, document, ShareRole::Editor, None, tenant.owner, None).await.map_err(|error| format!("{error:?}"))?;
    t.commit().await?;
    Ok((link.id, link.secret))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn share_link_exchange_racing_revoke_linearises_at_the_revoke_commit() -> TestResult {
    let Some(url) = database_url()? else {
        return Ok(());
    };
    let pool = shared_pool(&url).await?;
    let tenant = tenant(&pool).await?;
    let (link, secret) = share_link(&pool, &tenant).await?;

    // The revoke is written but not committed: a concurrent exchange is not blocked and still
    // sees the live link (the revoke hasn't happened yet for anyone else).
    let mut revoking = tx(&pool, tenant.id).await?;
    assert_eq!(revoke_share_link(&mut revoking, link).await, Ok(true));
    let (exchanger, _) = racer_pool(&url).await?;
    let early = timeout(BOUND, exchange_share_token(&exchanger, &secret)).await?;
    assert_eq!(early.as_ref().map(|grant| grant.link_id), Ok(link), "{early:?}");
    assert!(dead_share_links(&pool, &[(tenant.id, link)]).await.map_err(|error| format!("{error:?}"))?.is_empty());

    // Many exchangers hammer the link while the revoke commits. Every exchange that started after
    // the commit returned is refused; the ones before may have succeeded.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut handles = Vec::new();
    for _ in 0..4 {
        let (racer, _) = racer_pool(&url).await?;
        let secret = secret.clone();
        let stop = stop.clone();
        handles.push(tokio::spawn(async move {
            let mut outcomes: Vec<(Instant, bool)> = Vec::new();
            while !stop.load(std::sync::atomic::Ordering::SeqCst) && outcomes.len() < 10_000 {
                let started = Instant::now();
                match exchange_share_token(&racer, &secret).await {
                    Ok(_) => outcomes.push((started, true)),
                    Err(ShareLinkError::Refused) => outcomes.push((started, false)),
                    Err(error) => return Err(format!("exchange failed: {error:?}").into()),
                }
            }
            Ok::<_, BoxError>(outcomes)
        }));
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    revoking.commit().await?;
    let committed = Instant::now();
    tokio::time::sleep(Duration::from_millis(100)).await;
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut before = 0;
    let mut after = 0;
    for handle in handles {
        for (started, succeeded) in join(handle).await? {
            if started > committed {
                after += 1;
                assert!(!succeeded, "an exchange that started after the revoke committed succeeded");
            } else if succeeded {
                before += 1;
            }
        }
    }
    assert!(before > 0 && after > 0, "the race didn't straddle the commit ({before} before, {after} after)");
    // Every grant handed out before the commit is dead to the relay's sweeper at once.
    assert_eq!(dead_share_links(&pool, &[(tenant.id, link)]).await.map_err(|error| format!("{error:?}"))?, vec![(tenant.id, link)]);
    assert_eq!(exchange_share_token(&pool, &secret).await.map(|grant| grant.link_id), Err(ShareLinkError::Refused));
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Browser sessions
// ---------------------------------------------------------------------------------------------

/// A live session for `principal` whose last touch is old enough that the next resolve writes.
async fn stale_session(pool: &PgPool, principal: PrincipalId) -> TestResult<(Uuid, String)> {
    let created = create_session(pool, principal, SessionLimits::default()).await.map_err(|error| format!("{error:?}"))?;
    sqlx::query("UPDATE browser_sessions SET last_seen_at = now() - interval '1 hour' WHERE id = $1::uuid")
        .bind(created.id.to_string())
        .execute(pool)
        .await?;
    Ok((created.id, created.token.clone()))
}

async fn session_row(pool: &PgPool, id: Uuid) -> TestResult<(bool, Option<String>)> {
    Ok(sqlx::query_as("SELECT revoked_at IS NOT NULL, revoke_reason FROM browser_sessions WHERE id = $1::uuid")
        .bind(id.to_string())
        .fetch_one(pool)
        .await?)
}

async fn live_sessions(pool: &PgPool, principal: PrincipalId) -> TestResult<i64> {
    Ok(sqlx::query_scalar("SELECT count(*) FROM browser_sessions WHERE principal_id = $1::uuid AND revoked_at IS NULL")
        .bind(principal.to_string())
        .fetch_one(pool)
        .await?)
}

/// A gate transaction holding session `id`'s row lock.
async fn lock_session(pool: &PgPool, id: Uuid) -> TestResult<sqlx::Transaction<'static, sqlx::Postgres>> {
    let mut gate = pool.begin().await?;
    sqlx::query("SELECT 1 FROM browser_sessions WHERE id = $1::uuid FOR UPDATE").bind(id.to_string()).execute(&mut *gate).await?;
    Ok(gate)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_touch_racing_logout_or_revoke_all_never_undoes_the_revoke() -> TestResult {
    let Some(url) = database_url()? else {
        return Ok(());
    };
    let pool = shared_pool(&url).await?;
    let limits = SessionLimits::default();
    for (revoke_all, revoker_first) in [(false, false), (false, true), (true, false), (true, true)] {
        let person = principal(&pool).await?;
        let (id, token) = stale_session(&pool, person).await?;
        let gate = lock_session(&pool, id).await?;

        // The resolver reads the live row (nothing has committed a revoke yet), then its touch
        // blocks on the gate; the revoker queues before or after it.
        let (resolver, resolver_pid) = racer_pool(&url).await?;
        let (revoker, revoker_pid) = racer_pool(&url).await?;
        let resolve_token = token.clone();
        let spawn_resolve = move || tokio::spawn(async move { Ok::<_, BoxError>(resolve_session(&resolver, &resolve_token, limits).await) });
        let revoke_token = token.clone();
        let spawn_revoke = move || {
            tokio::spawn(async move {
                Ok::<_, BoxError>(if revoke_all {
                    revoke_all_for_principal(&revoker, person, RevokeReason::MembershipRevoked).await.map(|count| count == 1)
                } else {
                    logout(&revoker, &revoke_token).await
                })
            })
        };
        let (resolving, revoking) = if revoker_first {
            let revoking = spawn_revoke();
            wait_until_blocked(&pool, &[revoker_pid]).await?;
            let resolving = spawn_resolve();
            wait_until_blocked(&pool, &[revoker_pid, resolver_pid]).await?;
            (resolving, revoking)
        } else {
            let resolving = spawn_resolve();
            wait_until_blocked(&pool, &[resolver_pid]).await?;
            let revoking = spawn_revoke();
            wait_until_blocked(&pool, &[resolver_pid, revoker_pid]).await?;
            (resolving, revoking)
        };
        gate.rollback().await?;

        let resolved = join(resolving).await?;
        let revoked = join(revoking).await?;
        // The resolve read the live row before any revoke committed, so it is linearised before
        // the revoke and succeeds; the revoke happened exactly once.
        assert_eq!(resolved.map(|session| session.id), Ok(id), "revoke_all={revoke_all} revoker_first={revoker_first}");
        assert_eq!(revoked, Ok(true));
        // Whichever order the writes ran in, the touch did not resurrect the session.
        let reason = if revoke_all { "membership_revoked" } else { "logout" };
        assert_eq!(session_row(&pool, id).await?, (true, Some(reason.to_string())));
        assert_eq!(resolve_session(&pool, &token, limits).await.map(|session| session.id), Err(SessionError::Refused));
        assert_eq!(logout(&pool, &token).await, Ok(false));
        assert_eq!(live_sessions(&pool, person).await?, 0);
    }
    Ok(())
}

/// Rotation racing "sign out everywhere": whichever takes the old session's row first, the
/// principal must end with no live session. Both arrival orders are forced.
///
/// Regression for the race this probe found: when the rotation won, the revoke-all statement's
/// snapshot predated the rotated session's insert and the new session survived. Rotation now
/// holds the principal `FOR SHARE` and revoke-all takes it `FOR UPDATE` before its UPDATE.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_rotation_racing_revoke_all_leaves_nothing_live() -> TestResult {
    rotation_race(true).await
}

/// The revoke-all-first order, which already holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revoke_all_winning_against_rotation_refuses_the_rotation() -> TestResult {
    rotation_race(false).await
}

async fn rotation_race(rotation_first: bool) -> TestResult {
    let Some(url) = database_url()? else {
        return Ok(());
    };
    let pool = shared_pool(&url).await?;
    let limits = SessionLimits::default();
    let person = principal(&pool).await?;
    let created = create_session(&pool, person, limits).await.map_err(|error| format!("{error:?}"))?;
    let gate = lock_session(&pool, created.id).await?;

    let (rotator, rotator_pid) = racer_pool(&url).await?;
    let (revoker, revoker_pid) = racer_pool(&url).await?;
    let token = created.token.clone();
    let spawn_rotate = move || tokio::spawn(async move { Ok::<_, BoxError>(rotate_session(&rotator, &token, limits).await) });
    let spawn_revoke =
        move || tokio::spawn(async move { Ok::<_, BoxError>(revoke_all_for_principal(&revoker, person, RevokeReason::Operator).await) });
    let (rotating, revoking) = if rotation_first {
        let rotating = spawn_rotate();
        wait_until_blocked(&pool, &[rotator_pid]).await?;
        let revoking = spawn_revoke();
        wait_until_blocked(&pool, &[rotator_pid, revoker_pid]).await?;
        (rotating, revoking)
    } else {
        let revoking = spawn_revoke();
        wait_until_blocked(&pool, &[revoker_pid]).await?;
        let rotating = spawn_rotate();
        wait_until_blocked(&pool, &[revoker_pid, rotator_pid]).await?;
        (rotating, revoking)
    };
    gate.rollback().await?;
    let rotated = join(rotating).await?;
    let revoked = join(revoking).await?;
    if rotation_first {
        let rotated = rotated.map_err(|error| format!("rotation lost although it arrived first: {error:?}"))?;
        assert!(revoked.is_ok(), "{revoked:?}");
        assert_eq!(resolve_session(&pool, &rotated.token, limits).await.map(|session| session.id), Err(SessionError::Refused));
    } else {
        assert_eq!(revoked, Ok(1));
        assert_eq!(rotated.map(|session| session.id), Err(SessionError::Refused));
    }
    assert_eq!(resolve_session(&pool, &created.token, limits).await.map(|session| session.id), Err(SessionError::Refused));
    assert_eq!(live_sessions(&pool, person).await?, 0, "a session survived sign-out-everywhere");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Service-account credentials
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn credential_authentication_racing_revoke_linearises_at_the_revoke_commit() -> TestResult {
    let Some(url) = database_url()? else {
        return Ok(());
    };
    let pool = shared_pool(&url).await?;
    let tenant = tenant(&pool).await?;
    let mut t = tx(&pool, tenant.id).await?;
    let account =
        create_service_account(&mut t, "d6 race robot", &[Scope::DocumentsRead], tenant.owner).await.map_err(|error| format!("{error:?}"))?;
    let issued = issue_credential(&mut t, account.id, 3600, tenant.owner).await.map_err(|error| format!("{error:?}"))?;
    t.commit().await?;
    let (authenticator, authenticator_pid) = racer_pool(&url).await?;
    assert_eq!(authenticate_credential(&authenticator, &issued.secret).await.map(|auth| auth.credential_id), Ok(issued.id));

    // Make last_used stale so the next authentication writes, then revoke without committing:
    // the authentication reads the live credential, and its touch blocks on the revoke's row lock.
    let mut t = tx(&pool, tenant.id).await?;
    sqlx::query("UPDATE service_account_credentials SET last_used_at = now() - interval '1 day' WHERE tenant_id = $1::uuid AND id = $2::uuid")
        .bind(tenant.id.to_string())
        .bind(issued.id.to_string())
        .execute(t.connection())
        .await?;
    t.commit().await?;
    let mut revoking = tx(&pool, tenant.id).await?;
    assert_eq!(revoke_credential(&mut revoking, account.id, issued.id, tenant.owner).await, Ok(true));
    let secret = issued.secret.clone();
    let authenticating = tokio::spawn(async move { Ok::<_, BoxError>(authenticate_credential(&authenticator, &secret).await) });
    wait_until_blocked(&pool, &[authenticator_pid]).await?;
    revoking.commit().await?;
    // The in-flight authentication read the credential before the revoke and completes (it is
    // linearised before it); its touch didn't undo the revoke.
    assert_eq!(join(authenticating).await?.map(|auth| auth.credential_id), Ok(issued.id));
    let mut t = tx(&pool, tenant.id).await?;
    let revoked: bool =
        sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM service_account_credentials WHERE tenant_id = $1::uuid AND id = $2::uuid")
            .bind(tenant.id.to_string())
            .bind(issued.id.to_string())
            .fetch_one(t.connection())
            .await?;
    t.rollback().await?;
    assert!(revoked);
    // Everything that starts after the commit is refused.
    assert_eq!(authenticate_credential(&pool, &issued.secret).await.map(|auth| auth.credential_id), Err(ServiceAccountError::Refused));
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Invitations
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invitation_accept_racing_revoke_has_exactly_one_winner_in_either_order() -> TestResult {
    let Some(url) = database_url()? else {
        return Ok(());
    };
    let pool = shared_pool(&url).await?;
    let tenant = tenant(&pool).await?;
    let team = d6();
    let mut t = tx(&pool, tenant.id).await?;
    create_team(&mut t, team, &format!("d6-{}", suffix()), "d6 race team", tenant.owner).await.map_err(|error| format!("{error:?}"))?;
    t.commit().await?;

    for accept_first in [true, false] {
        let invitee = principal(&pool).await?;
        let mut t = tx(&pool, tenant.id).await?;
        let invitation = create_invitation(&mut t, team, &format!("d6-{}@example.test", suffix()), TeamRole::Editor, tenant.owner, None)
            .await
            .map_err(|error| format!("{error:?}"))?;
        t.commit().await?;

        // The gate holds the invitation row; the racers arrive in the chosen order.
        let mut gate = tx(&pool, tenant.id).await?;
        sqlx::query("SELECT 1 FROM team_invitations WHERE tenant_id = $1::uuid AND id = $2::uuid FOR UPDATE")
            .bind(tenant.id.to_string())
            .bind(invitation.id.to_string())
            .execute(gate.connection())
            .await?;
        let (accepter, accepter_pid) = racer_pool(&url).await?;
        let (revoker, revoker_pid) = racer_pool(&url).await?;
        let token = invitation.token.clone();
        let (tenant_id, owner, invitation_id) = (tenant.id, tenant.owner, invitation.id);
        let spawn_accept = move || tokio::spawn(async move { Ok::<_, BoxError>(accept_invitation(&accepter, &token, invitee, None).await) });
        let spawn_revoke = move || {
            tokio::spawn(async move {
                let mut t = begin_tenant_transaction(&revoker, tenant_id).await.map_err(|error| format!("{error:?}"))?;
                let revoked = revoke_invitation(&mut t, invitation_id, owner).await;
                t.commit().await?;
                Ok::<_, BoxError>(revoked)
            })
        };
        let (accepting, revoking) = if accept_first {
            let accepting = spawn_accept();
            wait_until_blocked(&pool, &[accepter_pid]).await?;
            let revoking = spawn_revoke();
            wait_until_blocked(&pool, &[accepter_pid, revoker_pid]).await?;
            (accepting, revoking)
        } else {
            let revoking = spawn_revoke();
            wait_until_blocked(&pool, &[revoker_pid]).await?;
            let accepting = spawn_accept();
            wait_until_blocked(&pool, &[revoker_pid, accepter_pid]).await?;
            (accepting, revoking)
        };
        gate.rollback().await?;
        let accepted = join(accepting).await?;
        let revoked = join(revoking).await?;

        let mut t = tx(&pool, tenant.id).await?;
        let (accepted_at, revoked_at): (bool, bool) = sqlx::query_as(
            "SELECT accepted_at IS NOT NULL, revoked_at IS NOT NULL FROM team_invitations WHERE tenant_id = $1::uuid AND id = $2::uuid",
        )
        .bind(tenant.id.to_string())
        .bind(invitation.id.to_string())
        .fetch_one(t.connection())
        .await?;
        let team_member: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM team_members WHERE tenant_id = $1::uuid AND team_id = $2::uuid AND principal_id = $3::uuid AND state = 'active'",
        )
        .bind(tenant.id.to_string())
        .bind(team.to_string())
        .bind(invitee.to_string())
        .fetch_one(t.connection())
        .await?;
        t.rollback().await?;
        if accept_first {
            assert!(accepted.is_ok(), "{accepted:?}");
            assert_eq!(revoked, Ok(false));
            assert_eq!((accepted_at, revoked_at, team_member), (true, false, 1));
        } else {
            assert_eq!(accepted.map(|_| ()), Err(InvitationError::Refused));
            assert_eq!(revoked, Ok(true));
            assert_eq!((accepted_at, revoked_at, team_member), (false, true, 0));
        }
        // A replayed accept is refused either way.
        assert_eq!(accept_invitation(&pool, &invitation.token, invitee, None).await.map(|_| ()), Err(InvitationError::Refused));
    }
    Ok(())
}

//! S6-003 embed-token exchange against PostgreSQL (FIGCRAFT_TEST_DATABASE_URL_FILE, migrated
//! runtime URL). Fixture tenants and issuers are random, so runs are independent.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use figcraft_cloud_auth::embed_token::{EmbedError, Refusal, exchange_embed_token};
use figcraft_cloud_domain::{TenantId, Uuid};
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::embed_tokens::{EmbedTokenStoreError, consume_embed_jti, sweep_embed_jtis};
use figcraft_cloud_storage::host_systems::{HostRole, KeyAlgorithm, NewHostKey, NewHostSystem, add_host_key, register_host_system, set_host_enabled};
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::{Value, json};
use sqlx::postgres::{PgPool, PgPoolOptions};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

struct Host {
    tenant: TenantId,
    issuer: String,
    key: Ed25519KeyPair,
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| i64::try_from(d.as_secs()).unwrap_or(0)).unwrap_or(0)
}

async fn pool() -> TestResult<Option<PgPool>> {
    let Ok(file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; embed exchange probe skipped");
        return Ok(None);
    };
    let url = std::fs::read_to_string(file)?;
    Ok(Some(PgPoolOptions::new().max_connections(12).connect(url.trim_end()).await?))
}

async fn host(pool: &PgPool) -> TestResult<Host> {
    let tenant = TenantId::random();
    let issuer = format!("https://issuer-{}.example", Uuid::new_v4().simple());
    let rng = SystemRandom::new();
    let key = Ed25519KeyPair::from_pkcs8(Ed25519KeyPair::generate_pkcs8(&rng).map_err(|_| "keygen")?.as_ref()).map_err(|_| "pkcs8")?;
    let mut tx = begin_tenant_transaction(pool, tenant).await.map_err(|e| format!("{e:?}"))?;
    sqlx::query("INSERT INTO tenants (id, slug, name, plan_id, home_region) VALUES ($1::uuid, $2, 'embed probe', 'test', 'local')")
        .bind(tenant.to_string())
        .bind(format!("embed-{}", tenant.as_uuid().simple()))
        .execute(tx.connection())
        .await?;
    register_host_system(
        &mut tx,
        &NewHostSystem {
            system_id: "host-a".into(),
            issuer: issuer.clone(),
            audiences: vec!["figcraft".into()],
            allowed_origins: vec!["https://host.example".into()],
            max_role: HostRole::Editor,
            allow_insecure_localhost: false,
        },
    )
    .await?;
    add_host_key(
        &mut tx,
        "host-a",
        &NewHostKey { kid: "k1".into(), alg: KeyAlgorithm::EdDsa, public_key: key.public_key().as_ref().to_vec(), not_before: 0, expires_at: None },
    )
    .await?;
    tx.commit().await?;
    Ok(Host { tenant, issuer, key })
}

fn b64(value: &Value) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap_or_default())
}

fn token(host: &Host, jti: &str, tweak: impl FnOnce(&mut Value)) -> String {
    let t = now();
    let mut claims = json!({"iss": host.issuer, "aud": "figcraft", "sub": "user-1", "jti": jti, "exp": t + 120, "nbf": t - 5, "iat": t - 5,
        "tenant": "t1", "workspace": "w1", "document": "d1", "role": "editor"});
    tweak(&mut claims);
    let input = format!("{}.{}", b64(&json!({"alg": "EdDSA", "kid": "k1"})), b64(&claims));
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(host.key.sign(input.as_bytes()).as_ref()))
}

fn jti() -> String {
    Uuid::new_v4().simple().to_string()
}

#[tokio::test]
async fn a_token_works_once_and_a_replay_is_refused() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let host = host(&pool).await?;
    let token = token(&host, &jti(), |_| {});
    let verified = exchange_embed_token(&pool, &token, now()).await?;
    assert_eq!((verified.tenant_id, verified.system_id.as_str(), verified.claims.role), (host.tenant, "host-a", HostRole::Editor));
    assert_eq!(exchange_embed_token(&pool, &token, now()).await.err(), Some(EmbedError::Refused(Refusal::Replay)));
    // A different jti from the same host still works.
    exchange_embed_token(&pool, &self::token(&host, &jti(), |_| {}), now()).await?;
    Ok(())
}

#[tokio::test]
async fn concurrent_double_use_has_exactly_one_winner() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let host = host(&pool).await?;
    let token = token(&host, &jti(), |_| {});
    let mut tasks = Vec::new();
    for _ in 0..10 {
        let (pool, token) = (pool.clone(), token.clone());
        tasks.push(tokio::spawn(async move { exchange_embed_token(&pool, &token, now()).await }));
    }
    let (mut wins, mut replays) = (0, 0);
    for task in tasks {
        match task.await? {
            Ok(_) => wins += 1,
            Err(EmbedError::Refused(Refusal::Replay)) => replays += 1,
            Err(other) => return Err(format!("unexpected {other:?}").into()),
        }
    }
    assert_eq!((wins, replays), (1, 9));
    Ok(())
}

#[tokio::test]
async fn refusals_do_not_burn_tokens_and_a_disabled_host_is_refused() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let host = host(&pool).await?;
    let id = jti();
    // A refused token (wrong audience) must not consume its jti.
    let bad = token(&host, &id, |c| c["aud"] = json!("elsewhere"));
    assert_eq!(exchange_embed_token(&pool, &bad, now()).await.err(), Some(EmbedError::Refused(Refusal::Audience)));
    let expired = token(&host, &id, |c| c["exp"] = json!(now() - 100));
    assert_eq!(exchange_embed_token(&pool, &expired, now()).await.err(), Some(EmbedError::Refused(Refusal::Expired)));
    // An unknown issuer, garbage and oversize input.
    let stranger = token(&host, &jti(), |c| c["iss"] = json!("https://stranger.example"));
    assert_eq!(exchange_embed_token(&pool, &stranger, now()).await.err(), Some(EmbedError::Refused(Refusal::UnknownIssuer)));
    assert!(exchange_embed_token(&pool, "garbage", now()).await.is_err());
    assert_eq!(exchange_embed_token(&pool, &"a".repeat(5000), now()).await.err(), Some(EmbedError::Refused(Refusal::TooLarge)));
    // The same jti with a good token still works afterwards.
    exchange_embed_token(&pool, &token(&host, &id, |_| {}), now()).await?;

    let disabled = self::host(&pool).await?;
    let mut tx = begin_tenant_transaction(&pool, disabled.tenant).await.map_err(|e| format!("{e:?}"))?;
    assert!(set_host_enabled(&mut tx, "host-a", false).await?);
    tx.commit().await?;
    assert_eq!(exchange_embed_token(&pool, &token(&disabled, &jti(), |_| {}), now()).await.err(), Some(EmbedError::Refused(Refusal::UnknownIssuer)));
    Ok(())
}

#[tokio::test]
async fn consumed_ids_are_swept_after_expiry_and_isolated_by_tenant() -> TestResult {
    let Some(pool) = pool().await? else { return Ok(()) };
    let host = host(&pool).await?;
    let other = self::host(&pool).await?;
    let mut tx = begin_tenant_transaction(&pool, host.tenant).await.map_err(|e| format!("{e:?}"))?;
    assert!(consume_embed_jti(&mut tx, "host-a", "stale-0001", 1).await?);
    assert_eq!(sweep_embed_jtis(&mut tx, 100).await?, 1, "the expired id is swept");
    assert!(consume_embed_jti(&mut tx, "host-a", "fresh-0001", now() + 3_600).await?);
    // The fresh one is kept (a replay fails); an expired one is dropped by the next consume.
    assert!(!consume_embed_jti(&mut tx, "host-a", "fresh-0001", now() + 3_600).await?);
    for bad in ["short", "has space 123", &"j".repeat(129)] {
        assert_eq!(consume_embed_jti(&mut tx, "host-a", bad, 10).await.err(), Some(EmbedTokenStoreError::InvalidInput));
    }
    tx.commit().await?;

    // Another tenant cannot consume against this host (no such host there) nor see its rows.
    let mut tx = begin_tenant_transaction(&pool, other.tenant).await.map_err(|e| format!("{e:?}"))?;
    let visible: i64 = sqlx::query_scalar("SELECT count(*) FROM embed_token_uses WHERE jti = 'fresh-0001'").fetch_one(tx.connection()).await?;
    assert_eq!(visible, 0);
    tx.rollback().await?;
    let none: i64 = sqlx::query_scalar("SELECT count(*) FROM embed_token_uses").fetch_one(&pool).await?;
    assert_eq!(none, 0, "no tenant context sees no rows");
    let mut tx = begin_tenant_transaction(&pool, TenantId::random()).await.map_err(|e| format!("{e:?}"))?;
    assert_eq!(consume_embed_jti(&mut tx, "host-a", "fresh-0002", now() + 60).await.err(), Some(EmbedTokenStoreError::NotFound));
    tx.rollback().await?;
    Ok(())
}

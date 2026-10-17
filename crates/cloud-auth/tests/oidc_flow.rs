//! The OIDC login flow (S4-001) against an in-test mock provider on loopback and the dev
//! PostgreSQL (skipped without FIGCRAFT_TEST_DATABASE_URL_FILE).
//!
//! The mock serves discovery, a JWKS and a token endpoint. It checks the PKCE S256 challenge
//! from the authorization URL against the verifier the client redeems with, and signs ID tokens
//! with an RSA key generated in-test (RS256, kid `rsa-1`) or, after a key rotation, an ECDSA P-256
//! key (ES256, kid `ec-2`). Each mock binds a fresh port, so its issuer, and every identity it
//! asserts, is unique to the run; subjects are random.

// Test-only: generating the in-test signing keys has no meaningful recovery.
#![allow(clippy::panic)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::extract::{Form, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use figcraft_cloud_auth::oidc::{OidcConfig, OidcError, OidcProvider};
use figcraft_cloud_domain::Uuid;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair as _};
use rsa::signature::{SignatureEncoding as _, Signer as _};
use rsa::traits::PublicKeyParts as _;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use sqlx::postgres::{PgPool, PgPoolOptions};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const CLIENT_ID: &str = "figcraft-a2-test";
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

fn rsa_key() -> &'static rsa::RsaPrivateKey {
    static KEY: OnceLock<rsa::RsaPrivateKey> = OnceLock::new();
    KEY.get_or_init(|| {
        let mut rng = rsa::rand_core::OsRng;
        rsa::RsaPrivateKey::new(&mut rng, 2048).unwrap_or_else(|error| panic!("rsa key: {error}"))
    })
}

fn ec_key() -> &'static EcdsaKeyPair {
    static KEY: OnceLock<EcdsaKeyPair> = OnceLock::new();
    KEY.get_or_init(|| {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap_or_else(|error| panic!("ec key: {error}"));
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap_or_else(|error| panic!("ec key: {error}"))
    })
}

fn rsa_jwk() -> Value {
    let public = rsa_key().to_public_key();
    json!({"kty": "RSA", "use": "sig", "alg": "RS256", "kid": "rsa-1", "n": B64.encode(public.n().to_bytes_be()), "e": B64.encode(public.e().to_bytes_be())})
}

fn ec_jwk() -> Value {
    let point = ec_key().public_key().as_ref();
    let (x, y) = (point.get(1..33).unwrap_or_default(), point.get(33..65).unwrap_or_default());
    json!({"kty": "EC", "use": "sig", "alg": "ES256", "kid": "ec-2", "crv": "P-256", "x": B64.encode(x), "y": B64.encode(y)})
}

/// How the token endpoint signs or shapes the ID token.
#[derive(Clone, Copy, Debug)]
enum Mode {
    Valid,
    WrongNonce,
    WrongAudience,
    Expired,
    NotYetValid,
    AlgNone,
    Hs256,
    /// Signed with the ES256 key, which the JWKS only serves after rotation.
    RotatedKey,
    /// Signed with the ES256 key but naming a key id the provider never serves.
    UnknownKid,
}

struct Grant {
    nonce: String,
    challenge: String,
    subject: String,
    mode: Mode,
}

struct Mock {
    issuer: String,
    rotated: AtomicBool,
    jwks_fetches: AtomicUsize,
    grants: Mutex<HashMap<String, Grant>>,
}

fn sign(header: &Value, claims: &Value, mode: Mode) -> String {
    let input = format!("{}.{}", B64.encode(header.to_string()), B64.encode(claims.to_string()));
    let signature = match mode {
        Mode::AlgNone => Vec::new(),
        Mode::Hs256 => ring::hmac::sign(&ring::hmac::Key::new(ring::hmac::HMAC_SHA256, CLIENT_ID.as_bytes()), input.as_bytes()).as_ref().to_vec(),
        Mode::RotatedKey | Mode::UnknownKid => {
            let rng = ring::rand::SystemRandom::new();
            ec_key().sign(&rng, input.as_bytes()).map(|signature| signature.as_ref().to_vec()).unwrap_or_default()
        }
        _ => rsa::pkcs1v15::SigningKey::<Sha256>::new(rsa_key().clone()).sign(input.as_bytes()).to_vec(),
    };
    format!("{input}.{}", B64.encode(signature))
}

fn id_token(mock: &Mock, grant: &Grant) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).unwrap_or_default();
    let (alg, kid) = match grant.mode {
        Mode::AlgNone => ("none", "rsa-1"),
        Mode::Hs256 => ("HS256", "rsa-1"),
        Mode::RotatedKey => ("ES256", "ec-2"),
        Mode::UnknownKid => ("ES256", "ec-9"),
        _ => ("RS256", "rsa-1"),
    };
    let mut claims = json!({
        "iss": mock.issuer,
        "sub": grant.subject,
        "aud": CLIENT_ID,
        "iat": now,
        "exp": now + 300,
        "nonce": grant.nonce,
        "email": "a2-probe@example.test",
        "email_verified": true,
        "name": "A2 Probe",
    });
    match grant.mode {
        Mode::WrongNonce => claims["nonce"] = json!("not-the-nonce-that-was-sent"),
        Mode::WrongAudience => claims["aud"] = json!("someone-else"),
        Mode::Expired => {
            claims["iat"] = json!(now - 900);
            claims["exp"] = json!(now - 600);
        }
        Mode::NotYetValid => claims["nbf"] = json!(now + 600),
        _ => {}
    }
    sign(&json!({"alg": alg, "kid": kid, "typ": "JWT"}), &claims, grant.mode)
}

async fn discovery(State(mock): State<Arc<Mock>>) -> Json<Value> {
    Json(json!({
        "issuer": mock.issuer,
        "authorization_endpoint": format!("{}/authorize", mock.issuer),
        "token_endpoint": format!("{}/token", mock.issuer),
        "jwks_uri": format!("{}/jwks", mock.issuer),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256", "ES256"],
        "code_challenge_methods_supported": ["S256"],
    }))
}

async fn jwks(State(mock): State<Arc<Mock>>) -> Json<Value> {
    mock.jwks_fetches.fetch_add(1, Ordering::SeqCst);
    let keys = if mock.rotated.load(Ordering::SeqCst) { vec![rsa_jwk(), ec_jwk()] } else { vec![rsa_jwk()] };
    Json(json!({ "keys": keys }))
}

async fn token(State(mock): State<Arc<Mock>>, Form(form): Form<HashMap<String, String>>) -> (axum::http::StatusCode, Json<Value>) {
    let refused = (axum::http::StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"})));
    let (Some(code), Some(verifier)) = (form.get("code"), form.get("code_verifier")) else {
        return refused;
    };
    if form.get("grant_type").map(String::as_str) != Some("authorization_code") || form.get("client_id").map(String::as_str) != Some(CLIENT_ID) {
        return refused;
    }
    let Some(grant) = mock.grants.lock().ok().and_then(|mut grants| grants.remove(code)) else {
        return refused;
    };
    if B64.encode(Sha256::digest(verifier.as_bytes())) != grant.challenge {
        return refused;
    }
    let id_token = id_token(&mock, &grant);
    (axum::http::StatusCode::OK, Json(json!({"access_token": "a2-access", "token_type": "Bearer", "expires_in": 300, "id_token": id_token})))
}

async fn start_mock() -> Result<Arc<Mock>, Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let issuer = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    let mock = Arc::new(Mock { issuer, rotated: AtomicBool::new(false), jwks_fetches: AtomicUsize::new(0), grants: Mutex::new(HashMap::new()) });
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/jwks", get(jwks))
        .route("/token", post(token))
        .with_state(Arc::clone(&mock));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(mock)
}

fn config(mock: &Mock) -> OidcConfig {
    OidcConfig {
        allow_insecure_loopback: true,
        key_refresh_min_interval: Duration::from_secs(3600),
        ..OidcConfig::new(&mock.issuer, CLIENT_ID, None, "http://127.0.0.1:9/auth/callback")
    }
}

async fn pool() -> Result<Option<PgPool>, Box<dyn std::error::Error>> {
    let Ok(database_url_file) = std::env::var("FIGCRAFT_TEST_DATABASE_URL_FILE") else {
        eprintln!("FIGCRAFT_TEST_DATABASE_URL_FILE is unset; OIDC flow probe skipped");
        return Ok(None);
    };
    let database_url = std::fs::read_to_string(database_url_file)?;
    Ok(Some(PgPoolOptions::new().max_connections(4).connect(database_url.trim_end()).await?))
}

struct Started {
    state: String,
    code: String,
}

/// Begin a login, act as the browser at the provider (read the authorization request and issue a
/// code bound to its nonce and PKCE challenge) and return the callback parameters.
async fn start(provider: &OidcProvider, pool: &PgPool, mock: &Mock, subject: &str, mode: Mode) -> Result<Started, Box<dyn std::error::Error>> {
    let begin = provider.begin_login(pool, Some("/files/recent?tab=1")).await?;
    let url = url::Url::parse(&begin.authorization_url)?;
    assert!(url.as_str().starts_with(&format!("{}/authorize?", mock.issuer)));
    let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
    assert_eq!(query.get("response_type").map(String::as_str), Some("code"));
    assert_eq!(query.get("client_id").map(String::as_str), Some(CLIENT_ID));
    assert_eq!(query.get("code_challenge_method").map(String::as_str), Some("S256"));
    assert!(query.get("scope").is_some_and(|scope| scope.split(' ').any(|scope| scope == "openid")));
    let state = query.get("state").cloned().ok_or("no state")?;
    assert_eq!(state, begin.state_cookie);
    let nonce = query.get("nonce").cloned().ok_or("no nonce")?;
    let challenge = query.get("code_challenge").cloned().ok_or("no challenge")?;
    let code = format!("code-{}", Uuid::new_v4());
    mock.grants.lock().map_err(|_| "poisoned")?.insert(code.clone(), Grant { nonce, challenge, subject: subject.to_owned(), mode });
    Ok(Started { state, code })
}

fn subject() -> String {
    format!("a2-{}", Uuid::new_v4())
}

async fn login(
    provider: &OidcProvider,
    pool: &PgPool,
    mock: &Mock,
    mode: Mode,
) -> Result<Result<figcraft_cloud_auth::oidc::LoginOutcome, OidcError>, Box<dyn std::error::Error>> {
    let started = start(provider, pool, mock, &subject(), mode).await?;
    Ok(provider.complete_login(pool, &started.code, &started.state, Some(&started.state)).await)
}

#[tokio::test]
async fn first_login_creates_a_principal_and_the_next_one_reuses_it() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let mock = start_mock().await?;
    let provider = OidcProvider::discover(config(&mock)).await?;
    let subject = subject();
    let started = start(&provider, &pool, &mock, &subject, Mode::Valid).await?;
    let first = provider.complete_login(&pool, &started.code, &started.state, Some(&started.state)).await?;
    assert!(first.new_principal);
    assert_eq!(first.return_to.as_deref(), Some("/files/recent?tab=1"));

    let started = start(&provider, &pool, &mock, &subject, Mode::Valid).await?;
    let second = provider.complete_login(&pool, &started.code, &started.state, Some(&started.state)).await?;
    assert_eq!(second.principal_id, first.principal_id);
    assert!(!second.new_principal);

    let (kind, name, metadata): (String, String, Value) = sqlx::query_as(
        "SELECT p.kind, p.display_name, l.verified_metadata FROM identity_links l JOIN principals p ON p.id = l.principal_id \
         WHERE l.issuer = $1 AND l.subject = $2",
    )
    .bind(&mock.issuer)
    .bind(&subject)
    .fetch_one(&pool)
    .await?;
    assert_eq!((kind.as_str(), name.as_str()), ("user", "A2 Probe"));
    assert_eq!(metadata, json!({"email": "a2-probe@example.test", "email_verified": true, "name": "A2 Probe"}));
    Ok(())
}

#[tokio::test]
async fn invalid_id_tokens_are_rejected() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let mock = start_mock().await?;
    let provider = OidcProvider::discover(config(&mock)).await?;
    for mode in [Mode::WrongNonce, Mode::WrongAudience, Mode::Expired, Mode::NotYetValid, Mode::AlgNone, Mode::Hs256] {
        let outcome = login(&provider, &pool, &mock, mode).await?;
        // `none` may already fail to parse as a token response; either way it is unauthenticated.
        let error = outcome.err().ok_or(format!("{mode:?} was accepted"))?;
        if matches!(mode, Mode::AlgNone) {
            assert!(matches!(error, OidcError::InvalidIdToken | OidcError::TokenExchange), "{mode:?}: {error:?}");
        } else {
            assert_eq!(error, OidcError::InvalidIdToken, "{mode:?}");
        }
        assert_eq!(error.code(), figcraft_cloud_domain::ErrorCode::Unauthenticated);
    }
    Ok(())
}

#[tokio::test]
async fn a_state_is_single_use_bound_to_its_cookie_and_expires() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let mock = start_mock().await?;
    let provider = OidcProvider::discover(config(&mock)).await?;

    // Replay: the second use of a state is refused even with a fresh code.
    let started = start(&provider, &pool, &mock, &subject(), Mode::Valid).await?;
    provider.complete_login(&pool, &started.code, &started.state, Some(&started.state)).await?;
    assert_eq!(provider.complete_login(&pool, &started.code, &started.state, Some(&started.state)).await, Err(OidcError::Refused));

    // A callback whose state does not match the browser's cookie (or has none) is refused.
    let started = start(&provider, &pool, &mock, &subject(), Mode::Valid).await?;
    let other = start(&provider, &pool, &mock, &subject(), Mode::Valid).await?;
    assert_eq!(provider.complete_login(&pool, &started.code, &started.state, Some(&other.state)).await, Err(OidcError::Refused));
    assert_eq!(provider.complete_login(&pool, &started.code, &started.state, None).await, Err(OidcError::Refused));
    // An unknown, well-formed state is refused the same way.
    let forged = B64.encode([7u8; 32]);
    assert_eq!(provider.complete_login(&pool, &started.code, &forged, Some(&forged)).await, Err(OidcError::Refused));

    // Expiry: age the stored attempt past its ten minutes.
    let expired = start(&provider, &pool, &mock, &subject(), Mode::Valid).await?;
    let hash = Sha256::digest(expired.state.as_bytes()).to_vec();
    let aged = sqlx::query(
        "UPDATE oidc_login_attempts SET created_at = now() - interval '11 minutes', expires_at = now() - interval '1 minute' WHERE state_hash = $1",
    )
    .bind(&hash)
    .execute(&pool)
    .await?;
    assert_eq!(aged.rows_affected(), 1);
    assert_eq!(provider.complete_login(&pool, &expired.code, &expired.state, Some(&expired.state)).await, Err(OidcError::Refused));

    // Only the state's hash is stored.
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM oidc_login_attempts WHERE state_hash = $1 AND nonce <> $2 AND pkce_verifier <> $2")
        .bind(&hash)
        .bind(&expired.state)
        .fetch_one(&pool)
        .await?;
    assert_eq!(stored, 1);
    Ok(())
}

#[tokio::test]
async fn an_unknown_key_id_refreshes_the_keys_once_per_interval() -> TestResult {
    let Some(pool) = pool().await? else {
        return Ok(());
    };
    let mock = start_mock().await?;
    let provider = OidcProvider::discover(config(&mock)).await?;
    assert_eq!(mock.jwks_fetches.load(Ordering::SeqCst), 1);

    // The provider rotates in a new ES256 key; a token signed with it triggers one refresh.
    mock.rotated.store(true, Ordering::SeqCst);
    let rotated = login(&provider, &pool, &mock, Mode::RotatedKey).await?;
    assert!(rotated.is_ok(), "{rotated:?}");
    assert_eq!(mock.jwks_fetches.load(Ordering::SeqCst), 2);
    // The refreshed keys serve later logins without fetching again.
    assert!(login(&provider, &pool, &mock, Mode::RotatedKey).await?.is_ok());
    assert!(login(&provider, &pool, &mock, Mode::Valid).await?.is_ok());
    assert_eq!(mock.jwks_fetches.load(Ordering::SeqCst), 2);

    // Another unknown key id within the interval does not refetch and is rejected.
    assert_eq!(login(&provider, &pool, &mock, Mode::UnknownKid).await?, Err(OidcError::InvalidIdToken));
    assert_eq!(mock.jwks_fetches.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn discovery_enforces_the_issuer_and_the_endpoint_policy() -> TestResult {
    let mock = start_mock().await?;
    // The document's issuer must equal the configured one exactly.
    let mismatched = OidcConfig { issuer_url: mock.issuer.replace("127.0.0.1", "localhost"), ..config(&mock) };
    assert_eq!(OidcProvider::discover(mismatched).await.err(), Some(OidcError::Discovery));
    // Plain HTTP is refused outside the loopback test allowance.
    let production = OidcConfig { allow_insecure_loopback: false, ..config(&mock) };
    assert_eq!(OidcProvider::discover(production).await.err(), Some(OidcError::InvalidConfig));
    // A relative return_to is required.
    if let Some(pool) = pool().await? {
        let provider = OidcProvider::discover(config(&mock)).await?;
        assert_eq!(provider.begin_login(&pool, Some("https://evil.example/")).await.err(), Some(OidcError::InvalidReturnTo));
        assert_eq!(provider.begin_login(&pool, Some("//evil.example/")).await.err(), Some(OidcError::InvalidReturnTo));
    }
    Ok(())
}

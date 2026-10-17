//! Short-lived embed-token validation and one-use exchange (S6-003).
//!
//! A host backend signs a compact JWS (`header.claims.signature`) with a private key whose PUBLIC
//! half is registered in `host_systems`/`host_system_keys`. This module:
//!
//! * bounds the token size before any decoding;
//! * accepts only the algorithms `EdDSA` and `ES256` (no `none`, no HMAC, no `crit`), and only when
//!   the registered key for the header's `kid` uses that very algorithm (no algorithm confusion);
//! * verifies the signature with a key that is usable now (not-before, expiry, retirement);
//! * checks issuer, audience, expiry, not-before and issued-at with a small fixed clock skew, and a
//!   maximum token lifetime;
//! * consumes the `jti` atomically in PostgreSQL, so a token works exactly once.
//!
//! Every refusal reaches the caller as the same fixed [`EmbedError::Refused`]; the
//! [`Refusal`] category is for server-side metrics and logs only and never carries token text.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, ResourceState, Role, TenantId, Uuid};
use figcraft_cloud_storage::audit::record_audit_event;
use figcraft_cloud_storage::begin_tenant_transaction;
use figcraft_cloud_storage::effective_roles::resolve_document_access;
use figcraft_cloud_storage::embed_tokens::{EmbedTokenStoreError, consume_embed_jti, valid_jti};
use figcraft_cloud_storage::host_mappings::{HostMappingError, MappingKind, embed_boundary_holds, resolve_external_resource};
use figcraft_cloud_storage::host_systems::{HostKey, HostRole, HostSystem, KeyAlgorithm, find_host_by_issuer, get_host_system, list_host_keys};
use ring::signature;
use serde_json::Value;
use sqlx::PgPool;

/// Largest token accepted, checked before any decoding.
pub const MAX_TOKEN_BYTES: usize = 4096;
/// Longest validity a token may claim (`exp` minus `nbf`, and `exp` minus `iat`).
pub const MAX_LIFETIME_SECS: i64 = 300;
/// Clock skew tolerated in both directions between the host's clock and ours.
pub const CLOCK_SKEW_SECS: i64 = 30;
const MAX_CLAIM_BYTES: usize = 512;

/// Why a token was refused. Categories only; never includes token text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    Malformed,
    TooLarge,
    Algorithm,
    UnknownIssuer,
    UnknownKey,
    Signature,
    Audience,
    Expired,
    NotYetValid,
    Lifetime,
    Claims,
    Replay,
    /// The requested role exceeds the host's registered maximum (S6-004).
    Role,
    /// The tenant, workspace or document is not inside the host's mapped boundary (S6-004).
    Boundary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmbedError {
    /// The token is not acceptable. The reason is deliberately not part of the message.
    Refused(Refusal),
    /// A dependency failed; the caller may retry.
    Unavailable,
}

impl std::fmt::Display for EmbedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(_) => formatter.write_str("embed token refused"),
            Self::Unavailable => formatter.write_str("embed token service unavailable"),
        }
    }
}

impl std::error::Error for EmbedError {}

const fn refused<T>(reason: Refusal) -> Result<T, EmbedError> {
    Err(EmbedError::Refused(reason))
}

/// The validated claims of an embed token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmbedClaims {
    pub issuer: String,
    pub audiences: Vec<String>,
    pub subject: String,
    pub jti: String,
    pub expires_at: i64,
    pub not_before: i64,
    pub issued_at: i64,
    /// The host's own identifiers; they become FigCraft ids only through registered mappings.
    pub external_tenant: String,
    pub external_workspace: String,
    pub external_document: String,
    pub role: HostRole,
}

fn decode_part(part: &str, max_decoded: usize) -> Result<Value, EmbedError> {
    if part.is_empty() || part.len() > max_decoded * 2 {
        return refused(Refusal::Malformed);
    }
    let bytes = URL_SAFE_NO_PAD.decode(part).map_err(|_| EmbedError::Refused(Refusal::Malformed))?;
    serde_json::from_slice::<Value>(&bytes).ok().filter(Value::is_object).ok_or(EmbedError::Refused(Refusal::Malformed))
}

fn split(token: &str) -> Result<(&str, &str, &str), EmbedError> {
    if token.len() > MAX_TOKEN_BYTES {
        return refused(Refusal::TooLarge);
    }
    let mut parts = token.split('.');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(header), Some(claims), Some(signature), None) if !signature.is_empty() => Ok((header, claims, signature)),
        _ => refused(Refusal::Malformed),
    }
}

/// The unverified issuer, used only to find the registered host whose key will verify the token.
pub fn token_issuer(token: &str) -> Result<String, EmbedError> {
    let (_, claims, _) = split(token)?;
    let claims = decode_part(claims, MAX_TOKEN_BYTES)?;
    match claims.get("iss").and_then(Value::as_str) {
        Some(issuer) if !issuer.is_empty() && issuer.len() <= 4 * 256 => Ok(issuer.to_owned()),
        _ => refused(Refusal::UnknownIssuer),
    }
}

fn text_claim(claims: &Value, name: &str) -> Result<String, EmbedError> {
    match claims.get(name).and_then(Value::as_str) {
        Some(value) if !value.is_empty() && value.len() <= MAX_CLAIM_BYTES && !value.chars().any(char::is_control) => Ok(value.to_owned()),
        _ => refused(Refusal::Claims),
    }
}

fn time_claim(claims: &Value, name: &str) -> Result<i64, EmbedError> {
    match claims.get(name).and_then(Value::as_i64) {
        Some(value) if (0..=i64::from(i32::MAX) * 4).contains(&value) => Ok(value),
        _ => refused(Refusal::Claims),
    }
}

fn verify_signature(key: &HostKey, signing_input: &[u8], signature_bytes: &[u8]) -> bool {
    let algorithm: &dyn signature::VerificationAlgorithm = match key.alg {
        KeyAlgorithm::EdDsa => &signature::ED25519,
        KeyAlgorithm::Es256 => &signature::ECDSA_P256_SHA256_FIXED,
    };
    signature::UnparsedPublicKey::new(algorithm, &key.public_key).verify(signing_input, signature_bytes).is_ok()
}

/// Verify `token` against a registered host and its keys at `now` (Unix seconds). Pure: no I/O and
/// no replay check (see [`exchange_embed_token`] for `jti` consumption).
pub fn verify_embed_token(token: &str, host: &HostSystem, keys: &[HostKey], now: i64) -> Result<EmbedClaims, EmbedError> {
    let (header_part, claims_part, signature_part) = split(token)?;
    let header = decode_part(header_part, 1024)?;
    // Algorithm allow-list. `alg: none`, HMAC, RSA and anything else never reach a key, and a
    // header carrying `crit` (extensions we do not implement) is refused outright.
    let header_alg = match header.get("alg").and_then(Value::as_str).and_then(KeyAlgorithm::parse) {
        Some(alg) => alg,
        None => return refused(Refusal::Algorithm),
    };
    if header.get("crit").is_some() || header.get("typ").is_some_and(|typ| typ.as_str() != Some("JWT")) {
        return refused(Refusal::Malformed);
    }
    let Some(kid) = header.get("kid").and_then(Value::as_str) else {
        return refused(Refusal::UnknownKey);
    };
    let Some(key) = keys.iter().find(|key| key.kid == kid) else {
        return refused(Refusal::UnknownKey);
    };
    // The registered key decides the algorithm; the token cannot pick another one for it.
    if key.alg != header_alg {
        return refused(Refusal::Algorithm);
    }
    if !key.usable_at(now) {
        return refused(Refusal::UnknownKey);
    }
    let signature_bytes = URL_SAFE_NO_PAD.decode(signature_part).map_err(|_| EmbedError::Refused(Refusal::Malformed))?;
    let signing_input = &token.as_bytes()[..header_part.len() + 1 + claims_part.len()];
    if !verify_signature(key, signing_input, &signature_bytes) {
        return refused(Refusal::Signature);
    }

    // The claims are authentic from here on.
    let claims = decode_part(claims_part, MAX_TOKEN_BYTES)?;
    let issuer = text_claim(&claims, "iss")?;
    if issuer != host.issuer {
        return refused(Refusal::UnknownIssuer);
    }
    let audiences: Vec<String> = match claims.get("aud") {
        Some(Value::String(audience)) => vec![audience.clone()],
        Some(Value::Array(values)) if !values.is_empty() && values.len() <= 8 => {
            values.iter().filter_map(|value| value.as_str().map(str::to_owned)).collect()
        }
        _ => return refused(Refusal::Audience),
    };
    if audiences.is_empty() || !audiences.iter().any(|audience| host.audiences.contains(audience)) {
        return refused(Refusal::Audience);
    }
    let expires_at = time_claim(&claims, "exp")?;
    let not_before = time_claim(&claims, "nbf")?;
    let issued_at = time_claim(&claims, "iat")?;
    if now >= expires_at.saturating_add(CLOCK_SKEW_SECS) {
        return refused(Refusal::Expired);
    }
    if now.saturating_add(CLOCK_SKEW_SECS) < not_before || now.saturating_add(CLOCK_SKEW_SECS) < issued_at {
        return refused(Refusal::NotYetValid);
    }
    if expires_at <= not_before
        || expires_at - not_before > MAX_LIFETIME_SECS
        || expires_at <= issued_at
        || expires_at - issued_at > MAX_LIFETIME_SECS
    {
        return refused(Refusal::Lifetime);
    }
    let jti = text_claim(&claims, "jti")?;
    if !valid_jti(&jti) {
        return refused(Refusal::Claims);
    }
    let role = HostRole::parse(&text_claim(&claims, "role")?).ok_or(EmbedError::Refused(Refusal::Claims))?;
    // S6-004: a token can never grant more than the host's registered maximum, whatever it claims.
    if role > host.max_role {
        return refused(Refusal::Role);
    }
    Ok(EmbedClaims {
        issuer,
        audiences,
        subject: text_claim(&claims, "sub")?,
        jti,
        expires_at,
        not_before,
        issued_at,
        external_tenant: text_claim(&claims, "tenant")?,
        external_workspace: text_claim(&claims, "workspace")?,
        external_document: text_claim(&claims, "document")?,
        role,
    })
}

/// S6 effective role: the token's role intersected with what the principal itself holds on the
/// document (tenant role, workspace and document grants, team grants). A principal with no access,
/// or whose document, project or workspace is not active, gets nothing: the token never creates
/// access, it only narrows it.
async fn effective_role(
    transaction: &mut figcraft_cloud_storage::TenantTransaction<'_>,
    token_role: HostRole,
    principal: Uuid,
    document: Uuid,
) -> Result<HostRole, EmbedError> {
    let (Ok(principal), Ok(document)) = (PrincipalId::try_from(principal), CloudDocumentId::try_from(document)) else {
        return refused(Refusal::Boundary);
    };
    let access = match resolve_document_access(transaction, principal, document).await {
        Ok(Some(access)) => access,
        Ok(None) => return refused(Refusal::Boundary),
        Err(_) => return Err(EmbedError::Unavailable),
    };
    let held = match (access.state, access.role) {
        (ResourceState::Active, Some(Role::TenantOwner | Role::TenantAdmin | Role::WorkspaceAdmin | Role::Editor)) => HostRole::Editor,
        (ResourceState::Active, Some(Role::Commenter)) => HostRole::Commenter,
        (ResourceState::Active, Some(Role::Viewer | Role::TeamManager)) => HostRole::Viewer,
        _ => return refused(Refusal::Boundary),
    };
    Ok(token_role.min(held))
}

/// A token that passed every check and whose `jti` is now spent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedEmbed {
    pub tenant_id: TenantId,
    pub system_id: String,
    /// The FigCraft resources the host's external ids map to, all inside `tenant_id`.
    pub workspace_id: Uuid,
    pub document_id: Uuid,
    pub principal_id: Uuid,
    /// The granted role: the lowest of the token's role, the host's registered maximum and the
    /// principal's own effective role on the document.
    pub role: HostRole,
    pub claims: EmbedClaims,
}

/// S6-004: resolve the token's external tenant, workspace, document and subject through this
/// host's registered mappings and require them to lie inside the host's tenant. Any unmapped or
/// out-of-boundary resource is the same refusal.
async fn resolve_boundary(
    transaction: &mut figcraft_cloud_storage::TenantTransaction<'_>,
    host: &HostSystem,
    claims: &EmbedClaims,
) -> Result<(Uuid, Uuid, Uuid), EmbedError> {
    let lookup = |result: Result<Option<Uuid>, HostMappingError>| match result {
        Ok(Some(id)) => Ok(id),
        Ok(None) => refused(Refusal::Boundary),
        Err(_) => Err(EmbedError::Unavailable),
    };
    let system = host.system_id.as_str();
    let tenant = lookup(resolve_external_resource(transaction, system, MappingKind::Tenant, &claims.external_tenant).await)?;
    if tenant != host.tenant_id.as_uuid() {
        return refused(Refusal::Boundary);
    }
    let workspace = lookup(resolve_external_resource(transaction, system, MappingKind::Workspace, &claims.external_workspace).await)?;
    let document = lookup(resolve_external_resource(transaction, system, MappingKind::Document, &claims.external_document).await)?;
    let principal = lookup(resolve_external_resource(transaction, system, MappingKind::Principal, &claims.subject).await)?;
    match embed_boundary_holds(transaction, workspace, document, principal).await {
        Ok(true) => Ok((workspace, document, principal)),
        Ok(false) => refused(Refusal::Boundary),
        Err(_) => Err(EmbedError::Unavailable),
    }
}

/// Validate `token` and consume its `jti`, in one tenant transaction (S6-003). The host is found
/// by its (enabled) issuer; a second presentation of the same token, concurrent or later, is
/// refused as a replay.
pub async fn exchange_embed_token(pool: &PgPool, token: &str, now: i64) -> Result<VerifiedEmbed, EmbedError> {
    let issuer = token_issuer(token)?;
    let located =
        find_host_by_issuer(pool, &issuer).await.map_err(|_| EmbedError::Unavailable)?.ok_or(EmbedError::Refused(Refusal::UnknownIssuer))?;
    let result = exchange_located(pool, &located, token, now).await;
    if let Err(EmbedError::Refused(reason)) = result {
        // The host is known, so the refusal is attributable to it: audit the category (never the
        // token) so a host's operator can see failed and replayed exchanges. Best effort: a
        // failing audit write must not turn a refusal into a different answer.
        record_denial(pool, &located, reason).await;
    }
    result
}

async fn record_denial(pool: &PgPool, located: &figcraft_cloud_storage::host_systems::HostLocator, reason: Refusal) {
    let Ok(mut transaction) = begin_tenant_transaction(pool, located.tenant_id).await else {
        return;
    };
    let metadata = serde_json::json!({"system_id": located.system_id, "reason": format!("{reason:?}").to_lowercase()});
    if record_audit_event(&mut transaction, Uuid::new_v4(), None, "embed.exchange", "host_system", None, None, None, "denied", &metadata)
        .await
        .is_ok()
    {
        let _ = transaction.commit().await;
    }
}

async fn exchange_located(
    pool: &PgPool,
    located: &figcraft_cloud_storage::host_systems::HostLocator,
    token: &str,
    now: i64,
) -> Result<VerifiedEmbed, EmbedError> {
    let mut transaction = begin_tenant_transaction(pool, located.tenant_id).await.map_err(|_| EmbedError::Unavailable)?;
    let host = get_host_system(&mut transaction, &located.system_id)
        .await
        .map_err(|_| EmbedError::Unavailable)?
        .filter(|host| host.enabled)
        .ok_or(EmbedError::Refused(Refusal::UnknownIssuer))?;
    let keys = list_host_keys(&mut transaction, &located.system_id).await.map_err(|_| EmbedError::Unavailable)?;
    let claims = verify_embed_token(token, &host, &keys, now)?;
    let (workspace_id, document_id, principal_id) = resolve_boundary(&mut transaction, &host, &claims).await?;
    let role = effective_role(&mut transaction, claims.role, principal_id, document_id).await?;
    // The row outlives the token's last possible validity (expiry plus skew).
    let keep_until = claims.expires_at.saturating_add(CLOCK_SKEW_SECS);
    match consume_embed_jti(&mut transaction, &located.system_id, &claims.jti, keep_until).await {
        Ok(true) => {}
        Ok(false) => return refused(Refusal::Replay),
        Err(EmbedTokenStoreError::InvalidInput) => return refused(Refusal::Claims),
        Err(_) => return Err(EmbedError::Unavailable),
    }
    let metadata = serde_json::json!({"system_id": located.system_id, "role": role.as_str(), "workspace_id": workspace_id.to_string()});
    record_audit_event(
        &mut transaction,
        Uuid::new_v4(),
        Some(principal_id),
        "embed.exchange",
        "document",
        Some(document_id),
        None,
        None,
        "succeeded",
        &metadata,
    )
    .await
    .map_err(|_| EmbedError::Unavailable)?;
    transaction.commit().await.map_err(|_| EmbedError::Unavailable)?;
    Ok(VerifiedEmbed { tenant_id: located.tenant_id, system_id: located.system_id.clone(), workspace_id, document_id, principal_id, role, claims })
}

#[cfg(test)]
mod tests {
    use super::*;
    use figcraft_cloud_storage::host_systems::HostRole;
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, Ed25519KeyPair, KeyPair};
    use serde_json::json;

    const NOW: i64 = 1_800_000_000;

    struct Signer {
        ed: Ed25519KeyPair,
        es: EcdsaKeyPair,
    }

    fn signer() -> Signer {
        let rng = SystemRandom::new();
        let ed = Ed25519KeyPair::from_pkcs8(Ed25519KeyPair::generate_pkcs8(&rng).expect("ed25519 key").as_ref()).expect("ed25519 pkcs8");
        let es = EcdsaKeyPair::from_pkcs8(
            &ECDSA_P256_SHA256_FIXED_SIGNING,
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).expect("p256 key").as_ref(),
            &rng,
        )
        .expect("p256 pkcs8");
        Signer { ed, es }
    }

    fn registered(signer: &Signer) -> (HostSystem, Vec<HostKey>) {
        let host = HostSystem {
            tenant_id: TenantId::random(),
            system_id: "host-a".into(),
            issuer: "https://host.example".into(),
            audiences: vec!["figcraft".into()],
            allowed_origins: vec![],
            max_role: HostRole::Editor,
            enabled: true,
        };
        let keys = vec![
            HostKey {
                kid: "ed".into(),
                alg: KeyAlgorithm::EdDsa,
                public_key: signer.ed.public_key().as_ref().to_vec(),
                not_before: 0,
                expires_at: None,
                retired_at: None,
            },
            HostKey {
                kid: "es".into(),
                alg: KeyAlgorithm::Es256,
                public_key: signer.es.public_key().as_ref().to_vec(),
                not_before: 0,
                expires_at: None,
                retired_at: None,
            },
        ];
        (host, keys)
    }

    fn claims() -> Value {
        json!({"iss": "https://host.example", "aud": "figcraft", "sub": "user-1", "jti": "jti-0000001", "exp": NOW + 120, "nbf": NOW - 5, "iat": NOW - 5,
               "tenant": "t1", "workspace": "w1", "document": "d1", "role": "editor"})
    }

    fn b64(value: &Value) -> String {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).expect("json"))
    }

    fn sign_ed(signer: &Signer, header: &Value, claims: &Value) -> String {
        let input = format!("{}.{}", b64(header), b64(claims));
        let sig = signer.ed.sign(input.as_bytes());
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig.as_ref()))
    }

    fn sign_es(signer: &Signer, header: &Value, claims: &Value) -> String {
        let input = format!("{}.{}", b64(header), b64(claims));
        let sig = signer.es.sign(&SystemRandom::new(), input.as_bytes()).expect("sign");
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig.as_ref()))
    }

    fn ed_header() -> Value {
        json!({"alg": "EdDSA", "kid": "ed", "typ": "JWT"})
    }

    fn reason(result: Result<EmbedClaims, EmbedError>) -> Refusal {
        match result {
            Err(EmbedError::Refused(reason)) => reason,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn valid_tokens_verify_with_both_algorithms() {
        let signer = signer();
        let (host, keys) = registered(&signer);
        let parsed = verify_embed_token(&sign_ed(&signer, &ed_header(), &claims()), &host, &keys, NOW).expect("ed25519 token");
        assert_eq!((parsed.subject.as_str(), parsed.role, parsed.external_document.as_str()), ("user-1", HostRole::Editor, "d1"));
        let es_token = sign_es(&signer, &json!({"alg": "ES256", "kid": "es"}), &claims());
        assert!(verify_embed_token(&es_token, &host, &keys, NOW).is_ok());
        let mut array_aud = claims();
        array_aud["aud"] = json!(["other", "figcraft"]);
        assert!(verify_embed_token(&sign_ed(&signer, &ed_header(), &array_aud), &host, &keys, NOW).is_ok());
    }

    #[test]
    fn identity_claims_are_checked() {
        let signer = signer();
        let (host, keys) = registered(&signer);
        let tweak = |name: &str, value: Value| {
            let mut c = claims();
            c[name] = value;
            reason(verify_embed_token(&sign_ed(&signer, &ed_header(), &c), &host, &keys, NOW))
        };
        assert_eq!(tweak("iss", json!("https://evil.example")), Refusal::UnknownIssuer);
        assert_eq!(tweak("aud", json!("someone-else")), Refusal::Audience);
        assert_eq!(tweak("aud", json!([])), Refusal::Audience);
        assert_eq!(tweak("aud", json!(5)), Refusal::Audience);
        assert_eq!(tweak("exp", json!(NOW - 31)), Refusal::Expired);
        assert_eq!(tweak("nbf", json!(NOW + 31)), Refusal::NotYetValid);
        assert_eq!(tweak("iat", json!(NOW + 31)), Refusal::NotYetValid);
        assert_eq!(tweak("exp", json!(NOW + 10_000)), Refusal::Lifetime);
        assert_eq!(tweak("nbf", json!(NOW - 1_000)), Refusal::Lifetime);
        assert_eq!(tweak("jti", json!("short")), Refusal::Claims);
        assert_eq!(tweak("jti", json!("has spaces in it")), Refusal::Claims);
        assert_eq!(tweak("role", json!("owner")), Refusal::Claims);
        assert_eq!(tweak("sub", json!("")), Refusal::Claims);
        assert_eq!(tweak("sub", json!("x".repeat(513))), Refusal::Claims);
        assert_eq!(tweak("exp", json!("soon")), Refusal::Claims);
        assert_eq!(tweak("document", json!(7)), Refusal::Claims);
        // Skew is tolerated within bounds, in both directions.
        assert!(
            verify_embed_token(
                &sign_ed(&signer, &ed_header(), &{
                    let mut c = claims();
                    c["exp"] = json!(NOW - 29);
                    c["nbf"] = json!(NOW - 100);
                    c["iat"] = json!(NOW - 100);
                    c
                }),
                &host,
                &keys,
                NOW
            )
            .is_ok()
        );
        assert!(
            verify_embed_token(
                &sign_ed(&signer, &ed_header(), &{
                    let mut c = claims();
                    c["nbf"] = json!(NOW + 29);
                    c["iat"] = json!(NOW + 29);
                    c
                }),
                &host,
                &keys,
                NOW
            )
            .is_ok()
        );
        // A missing claim is refused.
        for name in ["iss", "aud", "sub", "jti", "exp", "nbf", "iat", "tenant", "workspace", "document", "role"] {
            let mut c = claims();
            c.as_object_mut().expect("object").remove(name);
            assert!(verify_embed_token(&sign_ed(&signer, &ed_header(), &c), &host, &keys, NOW).is_err(), "{name} is required");
        }
    }

    #[test]
    fn signatures_algorithms_and_keys_are_enforced() {
        let signer = signer();
        let other = self::signer();
        let (host, keys) = registered(&signer);
        let good = sign_ed(&signer, &ed_header(), &claims());
        // Signed by someone else's key under a registered kid.
        assert_eq!(reason(verify_embed_token(&sign_ed(&other, &ed_header(), &claims()), &host, &keys, NOW)), Refusal::Signature);
        // Tampered claims keep the old signature.
        let parts: Vec<&str> = good.split('.').collect();
        let mut forged = claims();
        forged["role"] = json!("editor");
        forged["sub"] = json!("admin");
        let tampered = format!("{}.{}.{}", parts[0], b64(&forged), parts[2]);
        assert_eq!(reason(verify_embed_token(&tampered, &host, &keys, NOW)), Refusal::Signature);
        // Unknown kid and missing kid.
        assert_eq!(
            reason(verify_embed_token(&sign_ed(&signer, &json!({"alg": "EdDSA", "kid": "nope"}), &claims()), &host, &keys, NOW)),
            Refusal::UnknownKey
        );
        assert_eq!(reason(verify_embed_token(&sign_ed(&signer, &json!({"alg": "EdDSA"}), &claims()), &host, &keys, NOW)), Refusal::UnknownKey);
        // Algorithm confusion: `none`, HMAC, RSA, lowercase, and an allowed alg that does not match the key.
        for alg in ["none", "None", "HS256", "RS256", "eddsa", "ES256", ""] {
            let token = sign_ed(&signer, &json!({"alg": alg, "kid": "ed"}), &claims());
            assert_eq!(reason(verify_embed_token(&token, &host, &keys, NOW)), Refusal::Algorithm, "alg {alg:?}");
        }
        // An unsigned token and an empty signature.
        let unsigned = format!("{}.{}.", b64(&json!({"alg": "none"})), b64(&claims()));
        assert!(verify_embed_token(&unsigned, &host, &keys, NOW).is_err());
        assert_eq!(reason(verify_embed_token(&format!("{}.{}.", b64(&ed_header()), b64(&claims())), &host, &keys, NOW)), Refusal::Malformed);
        // HMAC attack: sign with HMAC-looking garbage using the public key as the secret.
        let hmac_like = format!("{}.{}.{}", b64(&json!({"alg": "HS256", "kid": "ed"})), b64(&claims()), URL_SAFE_NO_PAD.encode([1u8; 32]));
        assert_eq!(reason(verify_embed_token(&hmac_like, &host, &keys, NOW)), Refusal::Algorithm);
        // Extension headers are refused, and typ must be JWT when present.
        assert_eq!(
            reason(verify_embed_token(&sign_ed(&signer, &json!({"alg": "EdDSA", "kid": "ed", "crit": ["x"]}), &claims()), &host, &keys, NOW)),
            Refusal::Malformed
        );
        assert_eq!(
            reason(verify_embed_token(&sign_ed(&signer, &json!({"alg": "EdDSA", "kid": "ed", "typ": "evil"}), &claims()), &host, &keys, NOW)),
            Refusal::Malformed
        );
    }

    #[test]
    fn role_never_exceeds_the_hosts_maximum() {
        let signer = signer();
        let (mut host, keys) = registered(&signer);
        let token_for = |role: &str| {
            let mut c = claims();
            c["role"] = json!(role);
            sign_ed(&signer, &ed_header(), &c)
        };
        host.max_role = HostRole::Commenter;
        assert_eq!(reason(verify_embed_token(&token_for("editor"), &host, &keys, NOW)), Refusal::Role);
        assert_eq!(verify_embed_token(&token_for("commenter"), &host, &keys, NOW).map(|c| c.role), Ok(HostRole::Commenter));
        assert_eq!(verify_embed_token(&token_for("viewer"), &host, &keys, NOW).map(|c| c.role), Ok(HostRole::Viewer));
        host.max_role = HostRole::Viewer;
        assert_eq!(reason(verify_embed_token(&token_for("commenter"), &host, &keys, NOW)), Refusal::Role);
    }

    #[test]
    fn key_windows_gate_verification() {
        let signer = signer();
        let (host, mut keys) = registered(&signer);
        let token = sign_ed(&signer, &ed_header(), &claims());
        keys[0].not_before = NOW + 1;
        assert_eq!(reason(verify_embed_token(&token, &host, &keys, NOW)), Refusal::UnknownKey);
        keys[0].not_before = 0;
        keys[0].expires_at = Some(NOW);
        assert_eq!(reason(verify_embed_token(&token, &host, &keys, NOW)), Refusal::UnknownKey);
        keys[0].expires_at = Some(NOW + 1);
        assert!(verify_embed_token(&token, &host, &keys, NOW).is_ok());
        keys[0].retired_at = Some(NOW);
        assert_eq!(reason(verify_embed_token(&token, &host, &keys, NOW)), Refusal::UnknownKey);
    }

    #[test]
    fn malformed_and_oversize_tokens_are_refused() {
        let signer = signer();
        let (host, keys) = registered(&signer);
        let good = sign_ed(&signer, &ed_header(), &claims());
        assert_eq!(reason(verify_embed_token(&"a".repeat(MAX_TOKEN_BYTES + 1), &host, &keys, NOW)), Refusal::TooLarge);
        let mut padded = claims();
        padded["pad"] = json!("p".repeat(MAX_TOKEN_BYTES));
        assert_eq!(reason(verify_embed_token(&sign_ed(&signer, &ed_header(), &padded), &host, &keys, NOW)), Refusal::TooLarge);
        for bad in ["", ".", "..", "a.b", "a.b.c.d", "!!!.!!!.!!!", &format!("{good}.extra"), &format!(" {good}"), &good.replace('.', "..")] {
            assert!(verify_embed_token(bad, &host, &keys, NOW).is_err(), "{bad:?}");
        }
        // Header or claims that are not JSON objects.
        let not_object = format!("{}.{}.{}", URL_SAFE_NO_PAD.encode("[1]"), b64(&claims()), URL_SAFE_NO_PAD.encode([0u8; 64]));
        assert_eq!(reason(verify_embed_token(&not_object, &host, &keys, NOW)), Refusal::Malformed);
    }

    #[test]
    fn refusals_have_one_fixed_public_message() {
        let all = [
            Refusal::Malformed,
            Refusal::Signature,
            Refusal::Audience,
            Refusal::Expired,
            Refusal::Replay,
            Refusal::Role,
            Refusal::Boundary,
            Refusal::UnknownIssuer,
        ];
        for reason in all {
            assert_eq!(EmbedError::Refused(reason).to_string(), "embed token refused");
        }
    }
}

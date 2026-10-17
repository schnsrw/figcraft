//! Short-lived realtime tickets (S4-019): how a signed-in browser joins a document's relay room
//! without putting its session token or a share-link secret in the invite.
//!
//! `POST /api/v1/documents/{id}/realtime-ticket` mints a ticket for an authenticated member who
//! may join the document; the relay admits a hello whose token is a valid ticket for the hello's
//! file. Tickets are stateless: no table, nothing to clean up.
//!
//! # Wire format
//!
//! ```text
//! "rt1_" base64url-no-pad( version (1) | tenant (16) | principal (16) | session (16)
//!                          | access (1: 0 view, 1 edit) | expires-at unix seconds (8, BE) | tag (32) )
//! ```
//!
//! `tag` is HMAC-SHA256, keyed by a subkey derived from `SESSION_SIGNING_KEY`
//! ([`TicketKey::derive`], label [`KEY_LABEL`]), over the version-to-expiry bytes *followed by
//! the document id*: the document is bound by the MAC but not carried, which keeps a ticket at
//! [`TICKET_CHARS`] (124) characters, inside the collaboration invite's 128-character token limit
//! and its `[A-Za-z0-9_-]` alphabet. A ticket presented for any other document fails the MAC.
//!
//! Verification ([`verify`]) checks the exact length and prefix, the tag in constant time, the
//! version, the expiry (refused once past, and refused when it lies further ahead than
//! [`MAX_TICKET_SECS`] plus a small clock skew, so a key holder cannot mint long-lived tickets
//! the relay would honour). Every failure is the same [`TicketError::Invalid`].
//!
//! A ticket carries the browser session it was minted under, so the relay watches the admission
//! like any signed-in member (`revocation::dead_member_admissions`): revoking the session or the
//! member's access disconnects them within one sweep. Within its lifetime a ticket can be
//! presented more than once (it is a bearer credential for at most a minute); the relay re-checks
//! the session and the effective role on every admission, so a replay never outlives revocation.
//!
//! Nothing here logs a ticket, and [`TicketKey`]'s `Debug` is redacted.

use figcraft_cloud_domain::{CloudDocumentId, PrincipalId, TenantId, Uuid};
use sha2::{Digest as _, Sha256};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// The prefix every ticket starts with.
pub const TICKET_PREFIX: &str = "rt1_";
/// Longest lifetime a ticket may have (seconds).
pub const MAX_TICKET_SECS: u64 = 60;
/// Clock skew tolerated between the API that mints and the relay that verifies (seconds).
pub const CLOCK_SKEW_SECS: u64 = 5;
/// The label the ticket subkey is derived under.
pub const KEY_LABEL: &[u8] = b"figcraft realtime-ticket v1";
/// Shortest signing secret accepted (the same floor as the share-session key).
pub const MIN_SECRET_BYTES: usize = 16;

const VERSION: u8 = 1;
const TAG_BYTES: usize = 32;
/// version + tenant + principal + session + access + expiry.
const BODY_BYTES: usize = 1 + 16 + 16 + 16 + 1 + 8;
const TICKET_BYTES: usize = BODY_BYTES + TAG_BYTES;
/// Characters in every ticket (prefix included).
pub const TICKET_CHARS: usize = TICKET_PREFIX.len() + (TICKET_BYTES * 4).div_ceil(3);
// Collaboration invites carry tokens of at most 128 characters.
const _: () = assert!(TICKET_CHARS <= 128);

/// Why a ticket was not accepted. One variant on purpose: callers answer every failure alike.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TicketError {
    /// Malformed, forged, for another document, expired or too long-lived.
    Invalid,
    /// The signing secret is too short to key tickets.
    WeakSecret,
}

/// The HMAC key tickets are signed with.
#[derive(Clone)]
pub struct TicketKey([u8; 32]);

impl TicketKey {
    /// Derive the ticket subkey from `SESSION_SIGNING_KEY` (HMAC(secret, [`KEY_LABEL`])), so the
    /// session secret itself never signs tickets.
    ///
    /// # Errors
    /// [`TicketError::WeakSecret`] for a secret shorter than [`MIN_SECRET_BYTES`].
    pub fn derive(secret: &[u8]) -> Result<Self, TicketError> {
        if secret.len() < MIN_SECRET_BYTES {
            return Err(TicketError::WeakSecret);
        }
        Ok(Self(hmac_sha256(secret, &[KEY_LABEL])))
    }
}

impl std::fmt::Debug for TicketKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TicketKey(redacted)")
    }
}

/// What a ticket asserts: this principal, signed in with this session, may join this document of
/// this tenant, with edit access or not, until `expires_at`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RealtimeTicket {
    pub tenant: TenantId,
    pub document: CloudDocumentId,
    pub principal: PrincipalId,
    /// The browser session the ticket was minted under (`browser_sessions.id`).
    pub session: Uuid,
    /// Whether the principal held `document.edit` when the ticket was minted.
    pub edit: bool,
    /// Unix seconds.
    pub expires_at: u64,
}

/// HMAC-SHA256 (RFC 2104) over the concatenation of `parts`.
fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|byte| byte ^ 0x36));
    for part in parts {
        inner.update(part);
    }
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(block.map(|byte| byte ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

/// Constant-time equality (lengths are public).
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && left.iter().zip(right).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

fn body(ticket: &RealtimeTicket) -> [u8; BODY_BYTES] {
    let mut out = [0u8; BODY_BYTES];
    out[0] = VERSION;
    out[1..17].copy_from_slice(ticket.tenant.as_uuid().as_bytes());
    out[17..33].copy_from_slice(ticket.principal.as_uuid().as_bytes());
    out[33..49].copy_from_slice(ticket.session.as_bytes());
    out[49] = u8::from(ticket.edit);
    out[50..58].copy_from_slice(&ticket.expires_at.to_be_bytes());
    out
}

fn tag(key: &TicketKey, body: &[u8], document: CloudDocumentId) -> [u8; TAG_BYTES] {
    hmac_sha256(&key.0, &[body, document.as_uuid().as_bytes()])
}

/// Mint a ticket for `ticket` (whose `expires_at` the caller set at most [`MAX_TICKET_SECS`]
/// ahead; [`expiry`] computes it).
#[must_use]
pub fn mint(key: &TicketKey, ticket: &RealtimeTicket) -> String {
    let body = body(ticket);
    let mut bytes = [0u8; TICKET_BYTES];
    bytes[..BODY_BYTES].copy_from_slice(&body);
    bytes[BODY_BYTES..].copy_from_slice(&tag(key, &body, ticket.document));
    let mut out = String::with_capacity(TICKET_CHARS);
    out.push_str(TICKET_PREFIX);
    URL_SAFE_NO_PAD.encode_string(bytes, &mut out);
    out
}

/// The expiry for a ticket minted at `now` with `lifetime_secs`, capped at [`MAX_TICKET_SECS`]
/// (and at least one second).
#[must_use]
pub fn expiry(now: u64, lifetime_secs: u64) -> u64 {
    now.saturating_add(lifetime_secs.clamp(1, MAX_TICKET_SECS))
}

/// Whether `token` has a ticket's shape (prefix and exact length). Anything else, such as a
/// 43-character share-link secret, is not a ticket and is left to other admission paths.
#[must_use]
pub fn looks_like_ticket(token: &str) -> bool {
    token.len() == TICKET_CHARS && token.starts_with(TICKET_PREFIX)
}

fn id<T: TryFrom<Uuid>>(bytes: &[u8]) -> Result<T, TicketError> {
    let uuid = Uuid::from_slice(bytes).map_err(|_| TicketError::Invalid)?;
    T::try_from(uuid).map_err(|_| TicketError::Invalid)
}

/// Verify `token` as a ticket for `document` at `now` (unix seconds).
///
/// # Errors
/// [`TicketError::Invalid`] for anything but a live, untampered ticket for `document`.
pub fn verify(key: &TicketKey, token: &str, document: CloudDocumentId, now: u64) -> Result<RealtimeTicket, TicketError> {
    if !looks_like_ticket(token) {
        return Err(TicketError::Invalid);
    }
    let encoded = token.get(TICKET_PREFIX.len()..).ok_or(TicketError::Invalid)?;
    let bytes = URL_SAFE_NO_PAD.decode(encoded).map_err(|_| TicketError::Invalid)?;
    if bytes.len() != TICKET_BYTES {
        return Err(TicketError::Invalid);
    }
    let (body, presented) = bytes.split_at(BODY_BYTES);
    if !constant_time_eq(&tag(key, body, document), presented) {
        return Err(TicketError::Invalid);
    }
    if body.first() != Some(&VERSION) {
        return Err(TicketError::Invalid);
    }
    let edit = match body.get(49) {
        Some(0) => false,
        Some(1) => true,
        _ => return Err(TicketError::Invalid),
    };
    let expires_at = u64::from_be_bytes(body.get(50..58).and_then(|slice| slice.try_into().ok()).ok_or(TicketError::Invalid)?);
    if expires_at <= now || expires_at > now.saturating_add(MAX_TICKET_SECS + CLOCK_SKEW_SECS) {
        return Err(TicketError::Invalid);
    }
    Ok(RealtimeTicket {
        tenant: id(body.get(1..17).ok_or(TicketError::Invalid)?)?,
        document,
        principal: id(body.get(17..33).ok_or(TicketError::Invalid)?)?,
        session: Uuid::from_slice(body.get(33..49).ok_or(TicketError::Invalid)?).map_err(|_| TicketError::Invalid)?,
        edit,
        expires_at,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn key() -> TicketKey {
        TicketKey::derive(b"e1-realtime-ticket-test-secret").unwrap()
    }

    fn ticket(now: u64) -> RealtimeTicket {
        RealtimeTicket {
            tenant: TenantId::random(),
            document: CloudDocumentId::random(),
            principal: PrincipalId::random(),
            session: Uuid::new_v4(),
            edit: true,
            expires_at: expiry(now, MAX_TICKET_SECS),
        }
    }

    #[test]
    fn tickets_round_trip_and_fit_a_collaboration_invite() {
        let now = 1_800_000_000;
        let original = ticket(now);
        let token = mint(&key(), &original);
        assert_eq!(token.len(), TICKET_CHARS);
        assert!(token.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'));
        assert!(looks_like_ticket(&token));
        assert_eq!(verify(&key(), &token, original.document, now), Ok(original));
        let viewer = RealtimeTicket { edit: false, ..original };
        assert_eq!(verify(&key(), &mint(&key(), &viewer), original.document, now + 59), Ok(viewer));
        // The ticket names no secret: neither the session id nor the tenant shows in clear text.
        assert!(!token.contains(&original.session.to_string()));
    }

    #[test]
    fn tampered_forged_and_other_document_tickets_are_refused() {
        let now = 1_800_000_000;
        let original = ticket(now);
        let token = mint(&key(), &original);
        // Another document: the MAC covers the document.
        assert_eq!(verify(&key(), &token, CloudDocumentId::random(), now), Err(TicketError::Invalid));
        // Another key.
        let other = TicketKey::derive(b"another-signing-secret-0123").unwrap();
        assert_eq!(verify(&other, &token, original.document, now), Err(TicketError::Invalid));
        // Every single-character change is refused.
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        for index in TICKET_PREFIX.len()..token.len() {
            let mut bytes = token.clone().into_bytes();
            let current = bytes[index];
            bytes[index] = *alphabet.iter().find(|candidate| **candidate != current).unwrap();
            let changed = String::from_utf8(bytes).unwrap();
            assert_eq!(verify(&key(), &changed, original.document, now), Err(TicketError::Invalid), "byte {index}");
        }
        // Upgrading view to edit by flipping the access byte is a MAC failure.
        let viewer = mint(&key(), &RealtimeTicket { edit: false, ..original });
        let mut raw = URL_SAFE_NO_PAD.decode(&viewer[TICKET_PREFIX.len()..]).unwrap();
        raw[49] = 1;
        let upgraded = format!("{TICKET_PREFIX}{}", URL_SAFE_NO_PAD.encode(raw));
        assert_eq!(verify(&key(), &upgraded, original.document, now), Err(TicketError::Invalid));
        // Junk, share-link-shaped secrets and oversized input.
        for junk in ["", "rt1_", "rt1_AAAA", &"A".repeat(43), &format!("rt1_{}", "A".repeat(120)), &"x".repeat(4096)] {
            assert_eq!(verify(&key(), junk, original.document, now), Err(TicketError::Invalid), "{junk}");
        }
        assert!(!looks_like_ticket(&"A".repeat(43)));
    }

    #[test]
    fn expired_and_too_long_lived_tickets_are_refused() {
        let now = 1_800_000_000;
        let original = ticket(now);
        let token = mint(&key(), &original);
        assert!(verify(&key(), &token, original.document, original.expires_at - 1).is_ok());
        assert_eq!(verify(&key(), &token, original.document, original.expires_at), Err(TicketError::Invalid));
        assert_eq!(verify(&key(), &token, original.document, original.expires_at + 3600), Err(TicketError::Invalid));
        // A ticket expiring far ahead (signed with the right key) is refused all the same.
        let long = RealtimeTicket { expires_at: now + 3600, ..original };
        assert_eq!(verify(&key(), &mint(&key(), &long), original.document, now), Err(TicketError::Invalid));
        // Lifetimes are capped when minting.
        assert_eq!(expiry(now, 3600), now + MAX_TICKET_SECS);
        assert_eq!(expiry(now, 0), now + 1);
        assert_eq!(expiry(u64::MAX, 30), u64::MAX);
    }

    #[test]
    fn weak_secrets_are_refused_and_keys_are_redacted() {
        assert_eq!(TicketKey::derive(b"short").map(|_| ()), Err(TicketError::WeakSecret));
        assert_eq!(format!("{:?}", key()), "TicketKey(redacted)");
    }
}

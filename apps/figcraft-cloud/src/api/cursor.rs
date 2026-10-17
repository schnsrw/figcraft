//! Opaque, tamper-evident pagination cursors and bounded search (S4-020).
//!
//! # Cursors
//!
//! A list endpoint returns `next_cursor` when more rows exist. The client passes it back as
//! `?cursor=…` unchanged. A cursor is URL-safe base64 (no padding) of
//!
//! ```text
//! version (1) | kind length (1) | kind | filter hash (16) | issued-at unix seconds (8, BE)
//!   | row id (16) | sort-key length (2, BE) | sort key (UTF-8, ≤ MAX_SORT_KEY_BYTES) | tag (32)
//! ```
//!
//! where `tag` is HMAC-SHA256 over everything before it, keyed by a subkey derived from
//! `SESSION_SIGNING_KEY` ([`install_signing_secret`]). Decoding checks the length caps, the tag
//! (constant time), the version, the endpoint `kind` (a teams cursor is refused on the members
//! route), the filter hash (a cursor taken with `q=ab` is refused with `q=cd`) and the age
//! ([`CURSOR_TTL_SECS`]). Every failure is the same `400 invalid_cursor`. A cursor carries no
//! tenant identifier: list queries are always scoped by the tenant transaction, and the tenant is
//! part of the filter hash, so a cursor from another tenant is refused as foreign too.
//!
//! Until a signing secret is installed the process uses a random key, so cursors stop working
//! across restarts and replicas but are never forgeable.
//!
//! # Search
//!
//! [`SearchTerm`] is a trimmed, length-capped, control-free search string. [`SearchTerm::prefix_pattern`]
//! escapes `\`, `%` and `_` so user input can never widen an `ILIKE … ESCAPE '\'` match beyond a
//! literal prefix (or substring). Search queries are keyset paginated on `(sort key, id)` and
//! always run inside a tenant transaction.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use figcraft_cloud_domain::{ErrorCode, Uuid};
use sha2::{Digest as _, Sha256};

use super::error::ApiError;

/// Default page size when `limit` is absent.
pub(crate) const DEFAULT_PAGE_SIZE: u32 = 50;
/// Largest page size; larger requests are clamped.
pub(crate) const MAX_PAGE_SIZE: u32 = 100;
/// Cursors older than this are refused.
pub(crate) const CURSOR_TTL_SECS: u64 = 24 * 60 * 60;
/// Longest sort key carried in a cursor (bytes).
pub(crate) const MAX_SORT_KEY_BYTES: usize = 1024;
/// Longest accepted cursor text, checked before decoding.
pub(crate) const MAX_CURSOR_CHARS: usize = 2048;
/// Longest search term (characters, after trimming).
pub(crate) const MAX_SEARCH_CHARS: usize = 64;

const VERSION: u8 = 1;
const TAG_BYTES: usize = 32;
const FILTER_BYTES: usize = 16;
const MAX_KIND_BYTES: usize = 32;
const KEY_CONTEXT: &[u8] = b"figcraft/api/cursor/v1";

/// The HMAC key cursors are signed with.
#[derive(Clone)]
pub(crate) struct CursorKey([u8; 32]);

impl CursorKey {
    /// Derive the cursor subkey from the session signing secret (HMAC(secret, context)), so the
    /// session key itself never signs cursors.
    pub(crate) fn derive(secret: &[u8]) -> Self {
        Self(hmac_sha256(secret, &[KEY_CONTEXT]))
    }

    fn random() -> Self {
        let mut seed = [0u8; 32];
        seed[..16].copy_from_slice(Uuid::new_v4().as_bytes());
        seed[16..].copy_from_slice(Uuid::new_v4().as_bytes());
        Self::derive(&seed)
    }
}

impl std::fmt::Debug for CursorKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CursorKey(redacted)")
    }
}

static PROCESS_KEY: OnceLock<CursorKey> = OnceLock::new();

/// Install the process cursor key from `SESSION_SIGNING_KEY` (call once at startup, before
/// serving). Returns false when a key was already in place (the first one stays).
pub(crate) fn install_signing_secret(secret: &[u8]) -> bool {
    PROCESS_KEY.set(CursorKey::derive(secret)).is_ok()
}

/// The process cursor key (a random one when none was installed).
pub(crate) fn process_key() -> &'static CursorKey {
    PROCESS_KEY.get_or_init(CursorKey::random)
}

/// HMAC-SHA256 (RFC 2104) over the concatenation of `parts`.
pub(crate) fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
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

/// Constant-time equality of two byte strings (length is not secret).
pub(crate) fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter().zip(right).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// `400 invalid_cursor`.
pub(crate) const fn invalid_cursor() -> ApiError {
    ApiError::new(ErrorCode::InvalidCursor)
}

/// Where the next page starts: strictly after `(sort_key, id)`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Position {
    pub(crate) sort_key: String,
    pub(crate) id: Uuid,
}

/// Which list a cursor belongs to and the filter it was issued under.
#[derive(Clone, Debug)]
pub(crate) struct CursorScope {
    kind: &'static str,
    filter: [u8; FILTER_BYTES],
}

impl CursorScope {
    /// `kind` names the endpoint (e.g. `"teams"`); `filter` lists everything that shapes the
    /// result set (tenant, parent ids, search term), each part length-prefixed before hashing.
    pub(crate) fn new(kind: &'static str, filter: &[&str]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(kind.as_bytes());
        for part in filter {
            hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
            hasher.update(part.as_bytes());
        }
        let digest = hasher.finalize();
        let mut out = [0u8; FILTER_BYTES];
        out.copy_from_slice(&digest[..FILTER_BYTES]);
        Self { kind, filter: out }
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |duration| duration.as_secs())
}

/// Encode a cursor for `position` (sort keys longer than the cap are refused: `None`).
pub(crate) fn encode(key: &CursorKey, scope: &CursorScope, position: &Position, issued_at: u64) -> Option<String> {
    let kind = scope.kind.as_bytes();
    let sort = position.sort_key.as_bytes();
    if kind.is_empty() || kind.len() > MAX_KIND_BYTES || sort.len() > MAX_SORT_KEY_BYTES {
        return None;
    }
    let mut payload = Vec::with_capacity(1 + 1 + kind.len() + FILTER_BYTES + 8 + 16 + 2 + sort.len() + TAG_BYTES);
    payload.push(VERSION);
    payload.push(u8::try_from(kind.len()).ok()?);
    payload.extend_from_slice(kind);
    payload.extend_from_slice(&scope.filter);
    payload.extend_from_slice(&issued_at.to_be_bytes());
    payload.extend_from_slice(position.id.as_bytes());
    payload.extend_from_slice(&u16::try_from(sort.len()).ok()?.to_be_bytes());
    payload.extend_from_slice(sort);
    let tag = hmac_sha256(&key.0, &[&payload]);
    payload.extend_from_slice(&tag);
    Some(URL_SAFE_NO_PAD.encode(payload))
}

/// Decode and verify a cursor issued for `scope`, at time `now` (unix seconds).
pub(crate) fn decode(key: &CursorKey, scope: &CursorScope, cursor: &str, now: u64) -> Result<Position, ApiError> {
    if cursor.is_empty() || cursor.len() > MAX_CURSOR_CHARS {
        return Err(invalid_cursor());
    }
    let bytes = URL_SAFE_NO_PAD.decode(cursor).map_err(|_| invalid_cursor())?;
    let body_len = bytes.len().checked_sub(TAG_BYTES).ok_or_else(invalid_cursor)?;
    let (body, tag) = bytes.split_at(body_len);
    if !constant_time_eq(&hmac_sha256(&key.0, &[body]), tag) {
        return Err(invalid_cursor());
    }
    let mut reader = Reader(body);
    if reader.take(1)? != [VERSION] {
        return Err(invalid_cursor());
    }
    let kind_len = usize::from(*reader.take(1)?.first().ok_or_else(invalid_cursor)?);
    let kind = reader.take(kind_len)?;
    let filter = reader.take(FILTER_BYTES)?;
    if !constant_time_eq(kind, scope.kind.as_bytes()) || !constant_time_eq(filter, &scope.filter) {
        return Err(invalid_cursor());
    }
    let issued_at = u64::from_be_bytes(reader.take(8)?.try_into().map_err(|_| invalid_cursor())?);
    // Refuse expired cursors and cursors from the future (beyond a small clock skew).
    if issued_at > now.saturating_add(300) || now.saturating_sub(issued_at) > CURSOR_TTL_SECS {
        return Err(invalid_cursor());
    }
    let id = Uuid::from_slice(reader.take(16)?).map_err(|_| invalid_cursor())?;
    let sort_len = usize::from(u16::from_be_bytes(reader.take(2)?.try_into().map_err(|_| invalid_cursor())?));
    if sort_len > MAX_SORT_KEY_BYTES {
        return Err(invalid_cursor());
    }
    let sort_key = std::str::from_utf8(reader.take(sort_len)?).map_err(|_| invalid_cursor())?.to_owned();
    if !reader.0.is_empty() {
        return Err(invalid_cursor());
    }
    Ok(Position { sort_key, id })
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], ApiError> {
        if self.0.len() < count {
            return Err(invalid_cursor());
        }
        let (head, rest) = self.0.split_at(count);
        self.0 = rest;
        Ok(head)
    }
}

/// `limit` clamped to `1..=MAX_PAGE_SIZE` (absent: [`DEFAULT_PAGE_SIZE`]).
pub(crate) fn clamp_limit(limit: Option<u32>) -> u32 {
    limit.unwrap_or(DEFAULT_PAGE_SIZE).clamp(1, MAX_PAGE_SIZE)
}

/// The query string every list endpoint accepts.
#[derive(Debug, Default, serde::Deserialize)]
pub(crate) struct ListQuery {
    pub(crate) limit: Option<u32>,
    pub(crate) cursor: Option<String>,
    pub(crate) q: Option<String>,
}

/// A validated page request: the clamped limit, where to start, and the scope for the next cursor.
#[derive(Debug)]
pub(crate) struct PageRequest {
    pub(crate) limit: u32,
    pub(crate) after: Option<Position>,
    scope: CursorScope,
}

impl PageRequest {
    /// Validate `cursor` (if any) against `scope` with the process key.
    pub(crate) fn new(scope: CursorScope, limit: Option<u32>, cursor: Option<&str>) -> Result<Self, ApiError> {
        Self::with_key(process_key(), scope, limit, cursor, now_secs())
    }

    pub(crate) fn with_key(key: &CursorKey, scope: CursorScope, limit: Option<u32>, cursor: Option<&str>, now: u64) -> Result<Self, ApiError> {
        let after = cursor.map(|cursor| decode(key, &scope, cursor, now)).transpose()?;
        Ok(Self { limit: clamp_limit(limit), after, scope })
    }

    /// `LIMIT` for the query: one extra row tells whether another page exists.
    pub(crate) fn fetch_limit(&self) -> i64 {
        i64::from(self.limit) + 1
    }

    /// Trim `rows` to the page and build the next cursor from the last row kept.
    pub(crate) fn finish<T>(&self, mut rows: Vec<T>, position: impl Fn(&T) -> Position) -> (Vec<T>, Option<String>) {
        let limit = usize::try_from(self.limit).unwrap_or(usize::MAX);
        let more = rows.len() > limit;
        rows.truncate(limit);
        let next = if more { rows.last().and_then(|row| encode(process_key(), &self.scope, &position(row), now_secs())) } else { None };
        (rows, next)
    }

    /// The cursor for a page a repository already trimmed (its typed `next_cursor`).
    pub(crate) fn cursor_for(&self, position: &Position) -> Option<String> {
        encode(process_key(), &self.scope, position, now_secs())
    }

    /// The `after` position as SQL binds (`None` for the first page).
    pub(crate) fn after_binds(&self) -> (Option<String>, Option<String>) {
        match &self.after {
            Some(position) => (Some(position.sort_key.clone()), Some(position.id.to_string())),
            None => (None, None),
        }
    }
}

/// A bounded, validated search term.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SearchTerm(String);

impl SearchTerm {
    /// Trimmed `q`; empty means no search (`Ok(None)`). Longer than [`MAX_SEARCH_CHARS`] or
    /// containing control characters is `400 invalid_request`.
    pub(crate) fn parse(q: Option<&str>) -> Result<Option<Self>, ApiError> {
        let Some(q) = q.map(str::trim) else {
            return Ok(None);
        };
        if q.is_empty() {
            return Ok(None);
        }
        if q.chars().count() > MAX_SEARCH_CHARS || q.chars().any(char::is_control) {
            return Err(ApiError::new(ErrorCode::InvalidRequest));
        }
        Ok(Some(Self(q.to_owned())))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// `ILIKE` pattern matching names that start with the term, for `… ILIKE $n ESCAPE '\'`.
    pub(crate) fn prefix_pattern(&self) -> String {
        let mut pattern = escape_like(&self.0);
        pattern.push('%');
        pattern
    }
}

/// Escape `\`, `%` and `_` so the text matches literally in `LIKE`/`ILIKE … ESCAPE '\'`.
pub(crate) fn escape_like(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 4);
    for character in text.chars() {
        if matches!(character, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(character);
    }
    out
}

/// A page of results on the wire.
#[derive(Debug, serde::Serialize)]
pub(crate) struct PageBody<T: serde::Serialize> {
    pub(crate) items: Vec<T>,
    pub(crate) next_cursor: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> CursorKey {
        CursorKey::derive(b"test-session-signing-key")
    }

    fn position() -> Position {
        Position { sort_key: "Design team".to_owned(), id: Uuid::parse_str("d2d20000-0000-4000-8000-000000000001").unwrap() }
    }

    #[test]
    fn hmac_matches_rfc_4231_vectors() {
        // Test case 2.
        let tag = hmac_sha256(b"Jefe", &[b"what do ya want ", b"for nothing?"]);
        let hex: String = tag.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(hex, "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        // Test case 6: a key longer than the block is hashed first.
        let tag = hmac_sha256(&[0xaa; 131], &[b"Test Using Larger Than Block-Size Key - Hash Key First"]);
        let hex: String = tag.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(hex, "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54");
    }

    #[test]
    fn cursors_round_trip_and_are_opaque() {
        let scope = CursorScope::new("teams", &["tenant-a", "q"]);
        let cursor = encode(&key(), &scope, &position(), 1_000).unwrap();
        assert!(!cursor.contains("Design"), "cursor must not be readable as plain text");
        assert!(cursor.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'));
        assert_eq!(decode(&key(), &scope, &cursor, 1_010).unwrap(), position());
    }

    #[test]
    fn altered_foreign_and_expired_cursors_are_refused() {
        let scope = CursorScope::new("teams", &["tenant-a", ""]);
        let cursor = encode(&key(), &scope, &position(), 1_000).unwrap();
        let refused = |result: Result<Position, ApiError>| assert_eq!(result.unwrap_err(), invalid_cursor());
        // Every single-byte change of the decoded cursor is detected.
        let bytes = URL_SAFE_NO_PAD.decode(&cursor).unwrap();
        for index in 0..bytes.len() {
            let mut altered = bytes.clone();
            altered[index] ^= 0x01;
            refused(decode(&key(), &scope, &URL_SAFE_NO_PAD.encode(&altered), 1_000));
        }
        // Truncated, extended, junk, empty and oversized cursors.
        refused(decode(&key(), &scope, &cursor[..cursor.len() - 2], 1_000));
        refused(decode(&key(), &scope, &format!("{cursor}AA"), 1_000));
        refused(decode(&key(), &scope, "not a cursor!", 1_000));
        refused(decode(&key(), &scope, "", 1_000));
        refused(decode(&key(), &scope, &"A".repeat(MAX_CURSOR_CHARS + 1), 1_000));
        // Another key, endpoint, tenant or filter.
        refused(decode(&CursorKey::derive(b"other key"), &scope, &cursor, 1_000));
        refused(decode(&key(), &CursorScope::new("members", &["tenant-a", ""]), &cursor, 1_000));
        refused(decode(&key(), &CursorScope::new("teams", &["tenant-b", ""]), &cursor, 1_000));
        refused(decode(&key(), &CursorScope::new("teams", &["tenant-a", "x"]), &cursor, 1_000));
        // Filter parts are length-prefixed: ["ab", "c"] is not ["a", "bc"].
        let split = encode(&key(), &CursorScope::new("teams", &["ab", "c"]), &position(), 1_000).unwrap();
        refused(decode(&key(), &CursorScope::new("teams", &["a", "bc"]), &split, 1_000));
        // Expired and from the future.
        refused(decode(&key(), &scope, &cursor, 1_000 + CURSOR_TTL_SECS + 1));
        assert!(decode(&key(), &scope, &cursor, 1_000 + CURSOR_TTL_SECS).is_ok());
        let future = encode(&key(), &scope, &position(), 10_000).unwrap();
        refused(decode(&key(), &scope, &future, 1_000));
        assert_eq!(invalid_cursor().status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn oversized_sort_keys_are_not_encoded() {
        let scope = CursorScope::new("teams", &[]);
        let long = Position { sort_key: "x".repeat(MAX_SORT_KEY_BYTES + 1), ..position() };
        assert_eq!(encode(&key(), &scope, &long, 0), None);
        let max = Position { sort_key: "x".repeat(MAX_SORT_KEY_BYTES), ..position() };
        let cursor = encode(&key(), &scope, &max, 0).unwrap();
        assert!(cursor.len() <= MAX_CURSOR_CHARS);
        assert_eq!(decode(&key(), &scope, &cursor, 0).unwrap(), max);
    }

    #[test]
    fn page_sizes_are_clamped() {
        assert_eq!(clamp_limit(None), DEFAULT_PAGE_SIZE);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(1)), 1);
        assert_eq!(clamp_limit(Some(100)), 100);
        assert_eq!(clamp_limit(Some(101)), 100);
        assert_eq!(clamp_limit(Some(u32::MAX)), 100);
        let request = PageRequest::with_key(&key(), CursorScope::new("t", &[]), Some(2), None, 0).unwrap();
        let ids = [1u128, 2, 3].map(Uuid::from_u128);
        let (rows, next) = request.finish(ids.to_vec(), |id| Position { sort_key: String::new(), id: *id });
        assert_eq!(rows, ids[..2].to_vec());
        assert!(next.is_some());
        let (rows, next) = request.finish(ids[..2].to_vec(), |id| Position { sort_key: String::new(), id: *id });
        assert_eq!((rows.len(), next), (2, None));
    }

    #[test]
    fn search_terms_are_bounded_and_wildcards_escaped() {
        assert_eq!(SearchTerm::parse(None).unwrap(), None);
        assert_eq!(SearchTerm::parse(Some("   ")).unwrap(), None);
        assert_eq!(SearchTerm::parse(Some(" De ")).unwrap().unwrap().as_str(), "De");
        assert!(SearchTerm::parse(Some(&"a".repeat(MAX_SEARCH_CHARS))).is_ok());
        assert!(SearchTerm::parse(Some(&"a".repeat(MAX_SEARCH_CHARS + 1))).is_err());
        assert!(SearchTerm::parse(Some("a\u{0}b")).is_err());
        assert_eq!(SearchTerm::parse(Some("100%_\\x")).unwrap().unwrap().prefix_pattern(), "100\\%\\_\\\\x%");
        assert_eq!(escape_like("%"), "\\%");
        assert_eq!(escape_like("plain"), "plain");
    }
}

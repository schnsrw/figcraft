//! Browser-session policy (S4-002): lifetimes, rotation and the session cookie.
//!
//! Pure policy, no I/O. The storage repository (`figcraft_cloud_storage::sessions`) enforces the
//! lifetimes it is given; this module decides them and builds the `Set-Cookie` values.
//!
//! The cookie is `__Host-figcraft_session`: the `__Host-` prefix makes browsers require `Secure`,
//! `Path=/` and no `Domain`, so the cookie is bound to the exact origin. It is `HttpOnly` (no
//! script access) and `SameSite=Lax` (sent on top-level navigation back from the identity
//! provider, withheld from cross-site subrequests).

use std::time::Duration;

/// Cookie name.
pub const COOKIE_NAME: &str = "__Host-figcraft_session";
/// Default idle timeout.
pub const DEFAULT_IDLE: Duration = Duration::from_secs(8 * 60 * 60);
/// Default absolute lifetime.
pub const DEFAULT_ABSOLUTE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Default age after which a live session is rotated on its next request.
pub const DEFAULT_ROTATION_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Default minimum time between idle-expiry writes.
pub const DEFAULT_TOUCH_INTERVAL: Duration = Duration::from_secs(60);
/// Longest absolute lifetime a policy may set.
pub const MAX_ABSOLUTE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// Session tokens are exactly this many characters (URL-safe base64 of 32 bytes).
pub const TOKEN_CHARS: usize = 43;
/// Longest `Cookie` header scanned by [`session_token_from_cookie_header`].
pub const MAX_COOKIE_HEADER_BYTES: usize = 8 * 1024;

/// Why a policy is unusable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyError {
    /// Needs `0 < touch < idle ≤ absolute ≤ 30 days` and `0 < rotation ≤ absolute`.
    OutOfRange,
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("session policy out of range")
    }
}

impl std::error::Error for PolicyError {}

/// Why a session should be rotated now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RotationTrigger {
    /// The principal just authenticated.
    Login,
    /// The principal's roles or permissions changed.
    PrivilegeChange,
    /// The session is older than the rotation interval.
    Age,
}

/// Session lifetimes and rotation cadence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionPolicy {
    idle: Duration,
    absolute: Duration,
    rotation_interval: Duration,
    touch_interval: Duration,
}

impl Default for SessionPolicy {
    fn default() -> Self {
        Self { idle: DEFAULT_IDLE, absolute: DEFAULT_ABSOLUTE, rotation_interval: DEFAULT_ROTATION_INTERVAL, touch_interval: DEFAULT_TOUCH_INTERVAL }
    }
}

impl SessionPolicy {
    /// A checked policy.
    pub fn new(idle: Duration, absolute: Duration, rotation_interval: Duration, touch_interval: Duration) -> Result<Self, PolicyError> {
        let valid = !touch_interval.is_zero()
            && touch_interval < idle
            && idle <= absolute
            && absolute <= MAX_ABSOLUTE
            && !rotation_interval.is_zero()
            && rotation_interval <= absolute;
        if valid { Ok(Self { idle, absolute, rotation_interval, touch_interval }) } else { Err(PolicyError::OutOfRange) }
    }

    #[must_use]
    pub const fn idle(&self) -> Duration {
        self.idle
    }

    #[must_use]
    pub const fn absolute(&self) -> Duration {
        self.absolute
    }

    #[must_use]
    pub const fn rotation_interval(&self) -> Duration {
        self.rotation_interval
    }

    #[must_use]
    pub const fn touch_interval(&self) -> Duration {
        self.touch_interval
    }

    /// Whole seconds as the repository takes them: `(idle, absolute, touch)`. Bounded by
    /// [`MAX_ABSOLUTE`], so the conversion cannot overflow.
    #[must_use]
    pub fn storage_secs(&self) -> (i64, i64, i64) {
        let secs = |duration: Duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX);
        (secs(self.idle), secs(self.absolute), secs(self.touch_interval))
    }

    /// The reason to rotate a session of `age_secs` (as resolved), if any. Login and privilege
    /// changes always rotate; otherwise rotation is due past the rotation interval.
    #[must_use]
    pub fn rotation_due(&self, age_secs: i64, event: Option<RotationTrigger>) -> Option<RotationTrigger> {
        if let Some(event) = event {
            return Some(event);
        }
        let age = u64::try_from(age_secs).unwrap_or(0);
        (age >= self.rotation_interval.as_secs()).then_some(RotationTrigger::Age)
    }

    /// The `Set-Cookie` value carrying `token`, living `max_age_secs` (clamped to the policy's
    /// absolute lifetime and at least zero). `None` when the token is not a well-formed session
    /// token (so a header can never be injected).
    #[must_use]
    pub fn set_cookie(&self, token: &str, max_age_secs: i64) -> Option<String> {
        if !well_formed_token(token) {
            return None;
        }
        let max_age = u64::try_from(max_age_secs).unwrap_or(0).min(self.absolute.as_secs());
        Some(format!("{COOKIE_NAME}={token}; Max-Age={max_age}; Path=/; Secure; HttpOnly; SameSite=Lax"))
    }
}

/// The `Set-Cookie` value that clears the session cookie (logout).
#[must_use]
pub fn clear_cookie() -> String {
    format!("{COOKIE_NAME}=; Max-Age=0; Path=/; Secure; HttpOnly; SameSite=Lax")
}

fn well_formed_token(token: &str) -> bool {
    token.len() == TOKEN_CHARS && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The session token in a request `Cookie` header, if exactly one well-formed one is present.
/// Oversized headers, duplicates (a sign of cookie tossing) and malformed values yield `None`.
#[must_use]
pub fn session_token_from_cookie_header(header: &str) -> Option<&str> {
    if header.len() > MAX_COOKIE_HEADER_BYTES {
        return None;
    }
    let mut found = None;
    for pair in header.split(';') {
        let Some((name, value)) = pair.trim().split_once('=') else {
            continue;
        };
        if name.trim() != COOKIE_NAME {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(value.trim());
    }
    found.filter(|token| well_formed_token(token))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJ0123-_6";

    #[test]
    fn defaults_are_eight_hours_idle_and_seven_days_absolute() {
        let policy = SessionPolicy::default();
        assert_eq!(policy.idle(), Duration::from_secs(28_800));
        assert_eq!(policy.absolute(), Duration::from_secs(604_800));
        assert_eq!(policy.storage_secs(), (28_800, 604_800, 60));
        assert!(SessionPolicy::new(policy.idle(), policy.absolute(), policy.rotation_interval(), policy.touch_interval()).is_ok());
    }

    #[test]
    fn out_of_range_policies_are_rejected() {
        let h = Duration::from_secs(3600);
        for (idle, absolute, rotation, touch) in [
            (h, h, h, Duration::ZERO),
            (h, h, h, h),
            (h * 2, h, h, Duration::from_secs(1)),
            (h, MAX_ABSOLUTE + Duration::from_secs(1), h, Duration::from_secs(1)),
            (h, h, Duration::ZERO, Duration::from_secs(1)),
            (h, h, h * 2, Duration::from_secs(1)),
        ] {
            assert_eq!(SessionPolicy::new(idle, absolute, rotation, touch), Err(PolicyError::OutOfRange));
        }
    }

    #[test]
    fn rotation_after_login_privilege_change_and_age() {
        let policy = SessionPolicy::default();
        assert_eq!(policy.rotation_due(0, Some(RotationTrigger::Login)), Some(RotationTrigger::Login));
        assert_eq!(policy.rotation_due(5, Some(RotationTrigger::PrivilegeChange)), Some(RotationTrigger::PrivilegeChange));
        assert_eq!(policy.rotation_due(3599, None), None);
        assert_eq!(policy.rotation_due(3600, None), Some(RotationTrigger::Age));
        assert_eq!(policy.rotation_due(-5, None), None);
    }

    #[test]
    fn cookie_has_host_prefix_and_hardened_attributes() {
        let cookie = SessionPolicy::default().set_cookie(TOKEN, 100).unwrap();
        assert_eq!(cookie, format!("__Host-figcraft_session={TOKEN}; Max-Age=100; Path=/; Secure; HttpOnly; SameSite=Lax"));
        assert!(!cookie.contains("Domain"));
        assert!(SessionPolicy::default().set_cookie(TOKEN, i64::MAX).unwrap().contains("Max-Age=604800;"));
        assert!(SessionPolicy::default().set_cookie(TOKEN, -1).unwrap().contains("Max-Age=0;"));
        for bad in ["", "x; Domain=evil", "a\r\nSet-Cookie: x=y", &"a".repeat(44)] {
            assert_eq!(SessionPolicy::default().set_cookie(bad, 1), None);
        }
        assert!(clear_cookie().starts_with("__Host-figcraft_session=; Max-Age=0;"));
    }

    #[test]
    fn cookie_header_parsing_is_strict() {
        assert_eq!(session_token_from_cookie_header(&format!("a=b; {COOKIE_NAME}={TOKEN}; c=d")), Some(TOKEN));
        assert_eq!(session_token_from_cookie_header(&format!("{COOKIE_NAME}={TOKEN}")), Some(TOKEN));
        assert_eq!(session_token_from_cookie_header(&format!("{COOKIE_NAME}={TOKEN}; {COOKIE_NAME}={TOKEN}")), None);
        assert_eq!(session_token_from_cookie_header("figcraft_session=abc"), None);
        assert_eq!(session_token_from_cookie_header(&format!("{COOKIE_NAME}=short")), None);
        assert_eq!(session_token_from_cookie_header(&format!("x={}; {COOKIE_NAME}={TOKEN}", "y".repeat(MAX_COOKIE_HEADER_BYTES))), None);
        for junk in ["", ";;;", "=", "==;=", "\u{0}", "é=é", COOKIE_NAME] {
            assert_eq!(session_token_from_cookie_header(junk), None);
        }
    }
}

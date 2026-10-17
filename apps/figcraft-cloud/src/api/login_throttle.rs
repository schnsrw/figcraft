//! Per-login backoff for password sign-in (S6-035), the part that does not depend on whether the
//! account exists.
//!
//! The per-address token bucket (`rate_limit`) bounds one client; the database lockout
//! (`credentials::record_failure`) bounds guessing against an existing account across replicas. This
//! table closes the remaining gap: it counts failures per `(tenant, normalised login)` for *every*
//! login, known or not, and after the policy's threshold answers `429` with `Retry-After`
//! (exponential, capped), so the two cases cannot be told apart by the response. The key is a
//! SHA-256 of tenant and login, so the table holds no login text. It is per replica and bounded: when
//! full, the least recently used entry goes.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use figcraft_cloud_domain::TenantId;
use figcraft_cloud_storage::credentials::LockoutPolicy;
use sha2::{Digest, Sha256};

/// Entries kept at once.
const MAX_KEYS: usize = 20_000;
/// An entry with no failure for this long is forgotten.
const FORGET_AFTER: Duration = Duration::from_secs(3600);

#[derive(Clone, Copy, Debug)]
struct Entry {
    failures: u32,
    blocked_until: Option<Instant>,
    last_seen: Instant,
}

/// A bounded table of failure counters.
#[derive(Debug)]
pub(crate) struct LoginThrottle {
    policy: LockoutPolicy,
    entries: Mutex<HashMap<[u8; 32], Entry>>,
}

fn key(tenant: TenantId, login: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(tenant.as_uuid().as_bytes());
    hasher.update([0]);
    hasher.update(login.as_bytes());
    hasher.finalize().into()
}

impl LoginThrottle {
    pub(crate) fn new(policy: LockoutPolicy) -> Self {
        Self { policy, entries: Mutex::new(HashMap::new()) }
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<[u8; 32], Entry>> {
        self.entries.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `Some(wait)` while the login is blocked.
    pub(crate) fn blocked(&self, tenant: TenantId, login: &str, now: Instant) -> Option<Duration> {
        let entries = self.entries();
        let entry = entries.get(&key(tenant, login))?;
        entry.blocked_until.and_then(|until| until.checked_duration_since(now)).filter(|wait| !wait.is_zero())
    }

    /// Count a failure (only while not blocked; attempts made while blocked do not extend it).
    pub(crate) fn failed(&self, tenant: TenantId, login: &str, now: Instant) {
        let mut entries = self.entries();
        if entries.len() >= MAX_KEYS {
            entries.retain(|_, entry| now.saturating_duration_since(entry.last_seen) < FORGET_AFTER);
            while entries.len() >= MAX_KEYS {
                let Some(oldest) = entries.iter().min_by_key(|(_, entry)| entry.last_seen).map(|(key, _)| *key) else { break };
                entries.remove(&oldest);
            }
        }
        let policy = self.policy;
        let entry = entries.entry(key(tenant, login)).or_insert(Entry { failures: 0, blocked_until: None, last_seen: now });
        if entry.blocked_until.is_some_and(|until| until > now) {
            return;
        }
        if now.saturating_duration_since(entry.last_seen) >= FORGET_AFTER {
            entry.failures = 0;
        }
        entry.failures = entry.failures.saturating_add(1);
        entry.last_seen = now;
        if entry.failures >= policy.threshold {
            let shift = (entry.failures - policy.threshold).min(20);
            let secs = policy.base_secs.saturating_mul(1i64 << shift).min(policy.max_secs);
            entry.blocked_until = now.checked_add(Duration::from_secs(u64::try_from(secs).unwrap_or(0)));
        }
    }

    /// Forget the login after a successful sign-in.
    pub(crate) fn succeeded(&self, tenant: TenantId, login: &str) {
        self.entries().remove(&key(tenant, login));
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> LockoutPolicy {
        LockoutPolicy { threshold: 3, base_secs: 10, max_secs: 60 }
    }

    #[test]
    fn blocks_after_the_threshold_with_growing_waits_and_resets_on_success() {
        let throttle = LoginThrottle::new(policy());
        let tenant = TenantId::random();
        let t0 = Instant::now();
        for _ in 0..2 {
            throttle.failed(tenant, "ada", t0);
        }
        assert_eq!(throttle.blocked(tenant, "ada", t0), None);
        throttle.failed(tenant, "ada", t0);
        assert_eq!(throttle.blocked(tenant, "ada", t0), Some(Duration::from_secs(10)));
        // Attempts while blocked do not extend it.
        throttle.failed(tenant, "ada", t0 + Duration::from_secs(1));
        assert_eq!(throttle.blocked(tenant, "ada", t0 + Duration::from_secs(1)), Some(Duration::from_secs(9)));
        // After it lapses the next failure doubles the wait, up to the cap.
        let later = t0 + Duration::from_secs(11);
        assert_eq!(throttle.blocked(tenant, "ada", later), None);
        throttle.failed(tenant, "ada", later);
        assert_eq!(throttle.blocked(tenant, "ada", later), Some(Duration::from_secs(20)));
        let much_later = later + Duration::from_secs(21);
        for _ in 0..10 {
            throttle.failed(tenant, "ada", much_later);
        }
        assert!(throttle.blocked(tenant, "ada", much_later).is_some_and(|wait| wait <= Duration::from_secs(60)));
        // Other logins and tenants are unaffected; success clears.
        assert_eq!(throttle.blocked(tenant, "grace", t0), None);
        assert_eq!(throttle.blocked(TenantId::random(), "ada", t0), None);
        throttle.succeeded(tenant, "ada");
        assert_eq!(throttle.blocked(tenant, "ada", much_later), None);
    }

    #[test]
    fn the_table_is_bounded() {
        let throttle = LoginThrottle::new(policy());
        let tenant = TenantId::random();
        let now = Instant::now();
        for n in 0..(MAX_KEYS + 50) {
            throttle.failed(tenant, &format!("login-{n}"), now + Duration::from_millis(n as u64));
        }
        assert!(throttle.len() <= MAX_KEYS);
    }
}

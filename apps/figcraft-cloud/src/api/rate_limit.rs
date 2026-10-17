//! Authentication rate limits (S4-006): a token bucket per client address and route.
//!
//! The key is the client address from [`super::client_ip::resolve`] (IPv6 grouped by `/64`, since
//! one host usually holds a whole prefix) plus the matched route template, so a burst on the login
//! route does not exhaust the callback route. Memory is bounded: when the table is full, idle
//! (fully refilled) buckets are dropped first, then the least recently used one. A refused request
//! gets `429 rate_limited` with `Retry-After` and no detail about the limit or other clients.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::AppState;
use super::client_ip;
use super::error::ApiError;

/// Route templates longer than this share one bucket key.
const MAX_ROUTE_KEY_BYTES: usize = 128;

/// Bucket size, refill rate and table bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RateLimitConfig {
    /// Requests allowed back to back.
    pub(crate) burst: u32,
    /// One token returns after this long.
    pub(crate) refill_every: Duration,
    /// Distinct (client, route) buckets kept at once.
    pub(crate) max_keys: usize,
}

impl Default for RateLimitConfig {
    /// Ten attempts at once, then one every six seconds (ten a minute), 10 000 clients tracked.
    fn default() -> Self {
        Self { burst: 10, refill_every: Duration::from_secs(6), max_keys: 10_000 }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Key {
    client: IpAddr,
    route: String,
}

#[derive(Clone, Copy, Debug)]
struct Bucket {
    tokens: u32,
    /// When the next partial token started accruing.
    refilled_at: Instant,
    last_seen: Instant,
}

/// The decision for one request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Decision {
    Allowed,
    Limited { retry_after: Duration },
}

/// A bounded table of token buckets.
#[derive(Debug)]
pub(crate) struct RateLimiter {
    config: RateLimitConfig,
    buckets: Mutex<HashMap<Key, Bucket>>,
}

impl RateLimiter {
    pub(crate) fn new(config: RateLimitConfig) -> Self {
        let config = RateLimitConfig {
            burst: config.burst.max(1),
            refill_every: config.refill_every.max(Duration::from_millis(1)),
            max_keys: config.max_keys.max(1),
        };
        Self { config, buckets: Mutex::new(HashMap::new()) }
    }

    /// Take one token for `client` on `route` at `now`.
    pub(crate) fn check_at(&self, client: Option<IpAddr>, route: &str, now: Instant) -> Decision {
        let key = Key { client: group(client), route: route_key(route) };
        let mut buckets = self.buckets.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !buckets.contains_key(&key) && buckets.len() >= self.config.max_keys {
            self.evict(&mut buckets, now);
        }
        let config = self.config;
        let bucket = buckets.entry(key).or_insert(Bucket { tokens: config.burst, refilled_at: now, last_seen: now });
        refill(bucket, config, now);
        bucket.last_seen = now;
        if bucket.tokens > 0 {
            bucket.tokens -= 1;
            Decision::Allowed
        } else {
            let waited = now.saturating_duration_since(bucket.refilled_at);
            Decision::Limited { retry_after: config.refill_every.saturating_sub(waited) }
        }
    }

    fn evict(&self, buckets: &mut HashMap<Key, Bucket>, now: Instant) {
        let full_after = self.config.refill_every.saturating_mul(self.config.burst);
        buckets.retain(|_, bucket| now.saturating_duration_since(bucket.last_seen) < full_after);
        while buckets.len() >= self.config.max_keys {
            let Some(oldest) = buckets.iter().min_by_key(|(_, bucket)| bucket.last_seen).map(|(key, _)| key.clone()) else {
                break;
            };
            buckets.remove(&oldest);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buckets.lock().map_or(0, |buckets| buckets.len())
    }
}

fn refill(bucket: &mut Bucket, config: RateLimitConfig, now: Instant) {
    if bucket.tokens >= config.burst {
        bucket.refilled_at = now;
        return;
    }
    let elapsed = now.saturating_duration_since(bucket.refilled_at);
    let period = config.refill_every.as_nanos().max(1);
    let earned = elapsed.as_nanos() / period;
    if earned == 0 {
        return;
    }
    let earned_u32 = u32::try_from(earned).unwrap_or(u32::MAX);
    bucket.tokens = bucket.tokens.saturating_add(earned_u32).min(config.burst);
    if bucket.tokens >= config.burst {
        bucket.refilled_at = now;
    } else {
        // Keep the partial period already accrued.
        let used = u32::try_from(earned).map_or(Duration::MAX, |earned| config.refill_every.saturating_mul(earned));
        bucket.refilled_at = bucket.refilled_at.checked_add(used).unwrap_or(now);
    }
}

/// IPv4 (and IPv4-mapped) addresses are keyed whole, IPv6 by `/64`. Unknown peers share one key.
fn group(client: Option<IpAddr>) -> IpAddr {
    match client.map(client_ip::canonical) {
        Some(IpAddr::V6(v6)) => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & (u128::MAX << 64))),
        Some(address) => address,
        None => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
    }
}

fn route_key(route: &str) -> String {
    if route.len() <= MAX_ROUTE_KEY_BYTES { route.to_string() } else { "long".to_string() }
}

/// Seconds for `Retry-After`, rounded up, at least one.
pub(crate) fn retry_after_seconds(retry_after: Duration) -> u64 {
    let seconds = retry_after.as_secs();
    let seconds = if retry_after.subsec_nanos() > 0 { seconds.saturating_add(1) } else { seconds };
    seconds.max(1)
}

/// Middleware for the `/api/v1/auth` routes.
pub(crate) async fn auth_middleware(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let peer = request.extensions().get::<ConnectInfo<SocketAddr>>().map(|info| info.0.ip());
    let client = client_ip::resolve(peer, request.headers(), &state.config().trusted_proxies);
    let route = request.extensions().get::<MatchedPath>().map_or("/api/v1/auth/*", MatchedPath::as_str);
    match state.auth_limiter().check_at(client, route, Instant::now()) {
        Decision::Allowed => next.run(request).await,
        Decision::Limited { retry_after } => ApiError::rate_limited(retry_after_seconds(retry_after)).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter(burst: u32, refill_ms: u64, max_keys: usize) -> RateLimiter {
        RateLimiter::new(RateLimitConfig { burst, refill_every: Duration::from_millis(refill_ms), max_keys })
    }

    fn ip(value: &str) -> Option<IpAddr> {
        Some(value.parse().unwrap())
    }

    #[test]
    fn burst_then_limited_then_refills_one_token_per_period() {
        let limiter = limiter(3, 1000, 100);
        let start = Instant::now();
        for _ in 0..3 {
            assert_eq!(limiter.check_at(ip("192.0.2.1"), "/login", start), Decision::Allowed);
        }
        assert_eq!(limiter.check_at(ip("192.0.2.1"), "/login", start), Decision::Limited { retry_after: Duration::from_millis(1000) });
        let later = start + Duration::from_millis(400);
        assert_eq!(limiter.check_at(ip("192.0.2.1"), "/login", later), Decision::Limited { retry_after: Duration::from_millis(600) });
        let refilled = start + Duration::from_millis(1500);
        assert_eq!(limiter.check_at(ip("192.0.2.1"), "/login", refilled), Decision::Allowed);
        // The half period already accrued counts toward the next token.
        assert_eq!(limiter.check_at(ip("192.0.2.1"), "/login", refilled), Decision::Limited { retry_after: Duration::from_millis(500) });
        assert_eq!(limiter.check_at(ip("192.0.2.1"), "/login", start + Duration::from_millis(2000)), Decision::Allowed);
        // Long idleness refills to the burst, never beyond.
        let idle = start + Duration::from_secs(3600);
        for _ in 0..3 {
            assert_eq!(limiter.check_at(ip("192.0.2.1"), "/login", idle), Decision::Allowed);
        }
        assert!(matches!(limiter.check_at(ip("192.0.2.1"), "/login", idle), Decision::Limited { .. }));
    }

    #[test]
    fn buckets_are_separate_per_client_and_route() {
        let limiter = limiter(1, 60_000, 100);
        let now = Instant::now();
        assert_eq!(limiter.check_at(ip("192.0.2.1"), "/login", now), Decision::Allowed);
        assert!(matches!(limiter.check_at(ip("192.0.2.1"), "/login", now), Decision::Limited { .. }));
        assert_eq!(limiter.check_at(ip("192.0.2.2"), "/login", now), Decision::Allowed);
        assert_eq!(limiter.check_at(ip("192.0.2.1"), "/callback", now), Decision::Allowed);
        // IPv6 hosts in one /64 share a bucket; IPv4-mapped addresses match their IPv4 form.
        assert_eq!(limiter.check_at(ip("2001:db8::1"), "/login", now), Decision::Allowed);
        assert!(matches!(limiter.check_at(ip("2001:db8::ffff:2"), "/login", now), Decision::Limited { .. }));
        assert!(matches!(limiter.check_at(ip("::ffff:192.0.2.2"), "/login", now), Decision::Limited { .. }));
    }

    #[test]
    fn memory_is_bounded_and_idle_buckets_go_first() {
        let limiter = limiter(2, 1000, 4);
        let now = Instant::now();
        for host in 0..4 {
            limiter.check_at(ip(&format!("198.51.100.{host}")), "/login", now);
        }
        assert_eq!(limiter.len(), 4);
        // Client 0 is exhausted and recently seen; the newcomers evict the least recently used.
        let later = now + Duration::from_millis(10);
        limiter.check_at(ip("198.51.100.0"), "/login", later);
        limiter.check_at(ip("198.51.100.0"), "/login", later);
        for host in 10..200 {
            limiter.check_at(ip(&format!("198.51.100.{host}")), "/login", later + Duration::from_millis(u64::from(host as u8)));
            assert!(limiter.len() <= 4);
        }
        // After the burst period every bucket is idle and one sweep frees the table.
        let idle = later + Duration::from_secs(10);
        limiter.check_at(ip("203.0.113.1"), "/login", idle);
        assert_eq!(limiter.len(), 1);
    }

    #[test]
    fn retry_after_rounds_up_to_whole_seconds() {
        assert_eq!(retry_after_seconds(Duration::ZERO), 1);
        assert_eq!(retry_after_seconds(Duration::from_millis(1)), 1);
        assert_eq!(retry_after_seconds(Duration::from_millis(1001)), 2);
        assert_eq!(retry_after_seconds(Duration::from_secs(6)), 6);
    }

    #[test]
    fn degenerate_configs_are_clamped() {
        let limiter = limiter(0, 0, 0);
        let now = Instant::now();
        assert_eq!(limiter.check_at(None, "/login", now), Decision::Allowed);
        assert!(matches!(limiter.check_at(None, "/login", now), Decision::Limited { .. }));
        assert_eq!(limiter.check_at(None, &"x".repeat(1000), now), Decision::Allowed);
    }
}

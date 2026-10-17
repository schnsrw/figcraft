//! The password-hashing service the account routes share (S6-033).
//!
//! Argon2id takes tens of MiB and tens of milliseconds, so it never runs on an async worker and the
//! number of computations in flight is bounded by a semaphore (`FIGCRAFT_PASSWORD_CONCURRENCY`;
//! memory in use is at most that many times the configured memory cost). A request that cannot get a
//! slot within [`QUEUE_WAIT`] is answered `503` rather than queued without bound; the auth router's
//! per-address token buckets already limit how many a single client can submit.

use std::sync::Arc;
use std::time::Duration;

use figcraft_cloud_auth::password::{HashParams, PasswordError, PasswordHasher, Verification};
use tokio::sync::Semaphore;

/// The longest a request waits for a hashing slot.
pub(crate) const QUEUE_WAIT: Duration = Duration::from_secs(5);

/// Why a hashing call produced no answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ServiceError {
    /// The password broke the policy (safe to explain to the caller).
    Policy(figcraft_cloud_auth::password::PolicyViolation),
    /// No slot became free in time, or the blocking task failed.
    Unavailable,
}

/// A bounded, shareable password hasher.
#[derive(Clone)]
pub(crate) struct PasswordService {
    hasher: Arc<PasswordHasher>,
    gate: Arc<Semaphore>,
    wait: Duration,
}

impl std::fmt::Debug for PasswordService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("PasswordService").field("params", &self.hasher.params()).finish_non_exhaustive()
    }
}

impl PasswordService {
    /// A service for `params` allowing `concurrency` (1 to 64) computations at once.
    pub(crate) fn new(params: HashParams, concurrency: usize) -> Result<Self, PasswordError> {
        if !(1..=64).contains(&concurrency) {
            return Err(PasswordError::InvalidParams);
        }
        Ok(Self { hasher: Arc::new(PasswordHasher::new(params)?), gate: Arc::new(Semaphore::new(concurrency)), wait: QUEUE_WAIT })
    }

    pub(crate) fn params(&self) -> HashParams {
        self.hasher.params()
    }

    async fn run<T: Send + 'static>(&self, work: impl FnOnce(&PasswordHasher) -> T + Send + 'static) -> Result<T, ServiceError> {
        let permit = tokio::time::timeout(self.wait, Arc::clone(&self.gate).acquire_owned())
            .await
            .map_err(|_| ServiceError::Unavailable)?
            .map_err(|_| ServiceError::Unavailable)?;
        let hasher = Arc::clone(&self.hasher);
        let result = tokio::task::spawn_blocking(move || {
            let value = work(&hasher);
            drop(permit);
            value
        })
        .await;
        result.map_err(|_| ServiceError::Unavailable)
    }

    /// Hash a new password under the policy (`login` must not equal it).
    pub(crate) async fn hash(&self, password: String, login: Option<String>) -> Result<String, ServiceError> {
        self.run(move |hasher| hasher.hash(&password, login.as_deref())).await?.map_err(|error| match error {
            PasswordError::Policy(violation) => ServiceError::Policy(violation),
            PasswordError::InvalidParams | PasswordError::Internal => ServiceError::Unavailable,
        })
    }

    /// Hash a server-generated secret (no policy).
    pub(crate) async fn hash_unchecked(&self, secret: String) -> Result<String, ServiceError> {
        self.run(move |hasher| hasher.hash_unchecked(&secret)).await?.map_err(|_| ServiceError::Unavailable)
    }

    /// Check a password against a stored hash.
    pub(crate) async fn verify(&self, stored: String, password: String) -> Result<Verification, ServiceError> {
        self.run(move |hasher| hasher.verify(&stored, &password)).await
    }

    /// The same work for a login that does not exist.
    pub(crate) async fn verify_unknown(&self, password: String) -> Result<Verification, ServiceError> {
        self.run(move |hasher| hasher.verify_unknown(&password)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(concurrency: usize) -> PasswordService {
        PasswordService::new(HashParams { memory_kib: 8 * 1024, passes: 1, lanes: 1 }, concurrency).expect("service")
    }

    #[tokio::test]
    async fn hashes_and_verifies_off_the_async_workers() {
        let service = service(2);
        let hash = service.hash("a perfectly fine phrase".into(), None).await.expect("hash");
        assert_eq!(service.verify(hash.clone(), "a perfectly fine phrase".into()).await, Ok(Verification::Valid { needs_rehash: false }));
        assert_eq!(service.verify(hash, "another phrase entirely".into()).await, Ok(Verification::Invalid));
        assert_eq!(service.verify_unknown("whatever whatever".into()).await, Ok(Verification::Invalid));
    }

    #[tokio::test]
    async fn policy_violations_are_reported() {
        let service = service(1);
        assert_eq!(service.hash("short".into(), None).await, Err(ServiceError::Policy(figcraft_cloud_auth::password::PolicyViolation::TooShort)));
    }

    #[tokio::test]
    async fn concurrency_is_bounded() {
        assert!(PasswordService::new(HashParams::default(), 0).is_err());
        assert!(PasswordService::new(HashParams::default(), 65).is_err());
        let service = service(1);
        // Hold the only slot: a second call waits, then is refused.
        let permit = Arc::clone(&service.gate).acquire_owned().await.expect("permit");
        let mut impatient = service.clone();
        impatient.wait = Duration::from_millis(50);
        assert_eq!(impatient.verify_unknown("x".into()).await, Err(ServiceError::Unavailable));
        drop(permit);
    }
}

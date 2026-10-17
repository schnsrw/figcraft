//! Authenticated encryption for small secrets at rest (S6-013).
//!
//! AES-256-GCM with a random 96-bit nonce per seal. The key is a subkey derived from the
//! deployment's `SESSION_SIGNING_KEY` (HMAC-SHA256 under a fixed label), so no extra secret has to
//! be provisioned and the signing key itself never encrypts. The sealed form is
//! `version(1) || nonce(12) || ciphertext || tag(16)`; the caller binds it to its row with
//! associated data (tenant and endpoint ids), so a blob copied to another row or tenant does not
//! open. The key's `Debug` is redacted and nothing here logs plaintext.

use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};

/// Label the subkey is derived under. Changing it invalidates every sealed secret.
pub const KEY_LABEL: &[u8] = b"figcraft secret-box v1";
/// Shortest signing secret accepted (the same floor as the session and ticket keys).
pub const MIN_SECRET_BYTES: usize = 16;
/// The only sealed-format version so far.
pub const FORMAT_VERSION: u8 = 1;
const TAG_LEN: usize = 16;

/// Why sealing or opening failed. One variant per cause class, none carrying data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecretBoxError {
    /// The deployment secret is too short to derive a key from.
    WeakSecret,
    /// No secure random numbers.
    Random,
    /// The blob is malformed, for another row, or was tampered with.
    Invalid,
}

/// The AES-256-GCM key.
pub struct SecretKey(LessSafeKey);

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretKey(redacted)")
    }
}

impl SecretKey {
    /// Derive the key from `secret` (`SESSION_SIGNING_KEY`).
    ///
    /// # Errors
    /// [`SecretBoxError::WeakSecret`] for a secret shorter than [`MIN_SECRET_BYTES`].
    pub fn derive(secret: &[u8]) -> Result<Self, SecretBoxError> {
        if secret.len() < MIN_SECRET_BYTES {
            return Err(SecretBoxError::WeakSecret);
        }
        let derived = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, secret), KEY_LABEL);
        let unbound = UnboundKey::new(&AES_256_GCM, derived.as_ref()).map_err(|_| SecretBoxError::WeakSecret)?;
        Ok(Self(LessSafeKey::new(unbound)))
    }

    /// Seal `plaintext` bound to `aad`.
    ///
    /// # Errors
    /// [`SecretBoxError::Random`] when no nonce can be drawn.
    pub fn seal(&self, aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, SecretBoxError> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        SystemRandom::new().fill(&mut nonce_bytes).map_err(|_| SecretBoxError::Random)?;
        let mut buffer = plaintext.to_vec();
        self.0
            .seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce_bytes), Aad::from(aad), &mut buffer)
            .map_err(|_| SecretBoxError::Invalid)?;
        let mut sealed = Vec::with_capacity(1 + NONCE_LEN + buffer.len());
        sealed.push(FORMAT_VERSION);
        sealed.extend_from_slice(&nonce_bytes);
        sealed.extend_from_slice(&buffer);
        Ok(sealed)
    }

    /// Open a blob made by [`Self::seal`] with the same `aad`.
    ///
    /// # Errors
    /// [`SecretBoxError::Invalid`] for anything that does not authenticate.
    pub fn open(&self, aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, SecretBoxError> {
        if sealed.len() < 1 + NONCE_LEN + TAG_LEN || sealed.first() != Some(&FORMAT_VERSION) {
            return Err(SecretBoxError::Invalid);
        }
        let (nonce, body) = sealed[1..].split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce.try_into().map_err(|_| SecretBoxError::Invalid)?;
        let mut buffer = body.to_vec();
        let plaintext =
            self.0.open_in_place(Nonce::assume_unique_for_key(nonce), Aad::from(aad), &mut buffer).map_err(|_| SecretBoxError::Invalid)?;
        Ok(plaintext.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::{SecretBoxError, SecretKey};

    fn key() -> SecretKey {
        SecretKey::derive(b"a-long-enough-signing-secret").expect("key")
    }

    #[test]
    fn sealed_secrets_open_only_with_the_same_key_and_context() {
        let key = key();
        let sealed = key.seal(b"tenant|endpoint", b"whsec_example").expect("seal");
        assert_ne!(&sealed[13..], b"whsec_example");
        assert!(!sealed.windows(13).any(|window| window == b"whsec_example"));
        assert_eq!(key.open(b"tenant|endpoint", &sealed).expect("open"), b"whsec_example");
        // Another row, another key, a flipped bit, truncation and a bad version all fail alike.
        assert_eq!(key.open(b"tenant|other", &sealed), Err(SecretBoxError::Invalid));
        let other = SecretKey::derive(b"a-different-signing-secret").expect("key");
        assert_eq!(other.open(b"tenant|endpoint", &sealed), Err(SecretBoxError::Invalid));
        for index in 0..sealed.len() {
            let mut tampered = sealed.clone();
            tampered[index] ^= 1;
            assert_eq!(key.open(b"tenant|endpoint", &tampered), Err(SecretBoxError::Invalid), "byte {index}");
        }
        assert_eq!(key.open(b"tenant|endpoint", &sealed[..sealed.len() - 1]), Err(SecretBoxError::Invalid));
        assert_eq!(key.open(b"tenant|endpoint", &[]), Err(SecretBoxError::Invalid));
    }

    #[test]
    fn every_seal_uses_a_fresh_nonce() {
        let key = key();
        let first = key.seal(b"a", b"same").expect("seal");
        let second = key.seal(b"a", b"same").expect("seal");
        assert_ne!(first, second);
    }

    #[test]
    fn weak_secrets_are_refused_and_the_key_is_redacted() {
        assert_eq!(SecretKey::derive(b"short").err(), Some(SecretBoxError::WeakSecret));
        assert_eq!(format!("{:?}", key()), "SecretKey(redacted)");
    }
}

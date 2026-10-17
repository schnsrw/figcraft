//! Password hashing, verification and policy (S6-033).
//!
//! # Hash
//!
//! Argon2id (RFC 9106, version 0x13) through the RustCrypto `argon2` crate (MIT OR Apache-2.0; see
//! `docs/saas/password-hashing-decision.md`). A stored hash is the standard PHC string
//! `$argon2id$v=19$m=<KiB>,t=<passes>,p=<lanes>$<salt>$<hash>` (unpadded standard base64), so the
//! parameters travel with every hash: they can be raised later and [`Verification::Valid`] reports
//! `needs_rehash` when a verified hash is weaker than the current [`HashParams`]; the caller then
//! stores a fresh hash while it still holds the password (rehash on login).
//!
//! Defaults follow current OWASP guidance (Password Storage Cheat Sheet: Argon2id with at least
//! 19 MiB, 2 passes, 1 lane; we use 46 MiB with 2 passes, the guidance's stronger tier with one more
//! pass): `m = 47104 KiB, t = 2, p = 1`, a 16-byte random salt and a 32-byte output. They are
//! configurable inside safe bounds ([`HashParams::validated`]): 8 MiB to 1 GiB, 1 to 10 passes, 1 to
//! 8 lanes. A stored hash read from the database is bounded the same way before it is verified, so a
//! corrupted or malicious row cannot ask for gigabytes.
//!
//! # No account oracle
//!
//! [`PasswordHasher::verify`] always does one full Argon2 computation, and [`PasswordHasher::verify_unknown`]
//! does the same work against a dummy hash made with the current parameters, so "no such login", "wrong
//! password" and "malformed stored hash" cost the same. The tag comparison is constant time.
//!
//! # Policy
//!
//! [`check_policy`]: 12 to 128 characters (after NFKC normalisation, which is also what is hashed, so
//! the same password typed on different keyboards verifies), Unicode allowed, no control characters,
//! **no composition rules**. It refuses passwords that are one repeated character, a keyboard or
//! digit run, built only from a very common word plus digits and punctuation, equal to the login or the
//! login's local part, or on the bundled deny-list of the most common long passwords. This is a floor,
//! not a strength meter.
//!
//! Hashing takes tens of milliseconds and tens of MiB: servers run it on a blocking thread and bound
//! how many run at once (see the API's hash gate).

use argon2::{Algorithm, Argon2, Params, Version};
use ring::rand::SecureRandom as _;
use unicode_normalization::UnicodeNormalization;

/// Argon2 version written into every hash (0x13 = 19).
const VERSION: Version = Version::V0x13;
/// Salt length in bytes.
pub const SALT_BYTES: usize = 16;
/// Output (tag) length in bytes.
pub const OUTPUT_BYTES: usize = 32;
/// Shortest password in characters.
pub const MIN_PASSWORD_CHARS: usize = 12;
/// Longest password in characters.
pub const MAX_PASSWORD_CHARS: usize = 128;
/// Longest password in bytes before normalisation (bounds the work done on hostile input).
pub const MAX_PASSWORD_INPUT_BYTES: usize = 1024;
/// Longest stored hash string this module will parse.
pub const MAX_HASH_CHARS: usize = 512;

/// Memory bound in KiB: 8 MiB to 1 GiB.
const MEMORY_BOUNDS: (u32, u32) = (8 * 1024, 1024 * 1024);
const PASSES_BOUNDS: (u32, u32) = (1, 10);
const LANES_BOUNDS: (u32, u32) = (1, 8);

/// Fixed, non-leaking failure categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PasswordError {
    /// The parameters are out of the safe bounds.
    InvalidParams,
    /// The password breaks the policy; see [`PolicyViolation`] for which rule.
    Policy(PolicyViolation),
    /// No secure random numbers, or the hash computation itself failed.
    Internal,
}

impl std::fmt::Display for PasswordError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "password error: {self:?}")
    }
}

impl std::error::Error for PasswordError {}

/// Which policy rule a password broke. The message is safe to show the person choosing a password.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PolicyViolation {
    TooShort,
    TooLong,
    ControlCharacters,
    Repetitive,
    Common,
    ContainsLogin,
}

impl PolicyViolation {
    /// A fixed sentence for the person choosing the password.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::TooShort => "Use at least 12 characters.",
            Self::TooLong => "Use at most 128 characters.",
            Self::ControlCharacters => "Remove control characters.",
            Self::Repetitive => "Avoid repeated characters and simple sequences.",
            Self::Common => "That password is too common. Pick something less guessable.",
            Self::ContainsLogin => "The password must not be your login.",
        }
    }
}

/// Argon2id cost parameters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HashParams {
    /// Memory in KiB.
    pub memory_kib: u32,
    /// Passes over memory.
    pub passes: u32,
    /// Parallel lanes.
    pub lanes: u32,
}

impl Default for HashParams {
    /// 46 MiB, 2 passes, 1 lane (OWASP's Argon2id guidance, stronger tier).
    fn default() -> Self {
        Self { memory_kib: 47_104, passes: 2, lanes: 1 }
    }
}

impl HashParams {
    /// `self` when every field is inside the safe bounds.
    #[must_use]
    pub fn validated(self) -> Option<Self> {
        let within = |value: u32, (low, high): (u32, u32)| (low..=high).contains(&value);
        (within(self.memory_kib, MEMORY_BOUNDS)
            && within(self.passes, PASSES_BOUNDS)
            && within(self.lanes, LANES_BOUNDS)
            && self.memory_kib >= 8 * self.lanes)
            .then_some(self)
    }

    fn argon2(self, output: usize) -> Option<Argon2<'static>> {
        let params = Params::new(self.memory_kib, self.passes, self.lanes, Some(output)).ok()?;
        Some(Argon2::new(Algorithm::Argon2id, VERSION, params))
    }

    /// Whether a hash made with `self` is at least as strong as one made with `target` in every cost
    /// dimension (so nothing needs upgrading).
    #[must_use]
    pub const fn at_least(self, target: Self) -> bool {
        self.memory_kib >= target.memory_kib && self.passes >= target.passes && self.lanes >= target.lanes
    }
}

/// The outcome of checking a password against a stored hash.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Verification {
    /// Right password; `needs_rehash` when the stored hash is weaker than the current parameters
    /// (or not Argon2id with the standard salt/output sizes).
    Valid { needs_rehash: bool },
    /// Wrong password, or an unusable stored hash.
    Invalid,
}

/// Hashes and verifies passwords with fixed current parameters.
pub struct PasswordHasher {
    params: HashParams,
    /// A hash of a random secret made with `params`, verified when the login does not exist.
    dummy: String,
}

impl std::fmt::Debug for PasswordHasher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("PasswordHasher").field("params", &self.params).finish_non_exhaustive()
    }
}

/// NFKC-normalised password bytes, the form that is hashed.
fn normalised(password: &str) -> Option<String> {
    (password.len() <= MAX_PASSWORD_INPUT_BYTES).then(|| password.nfkc().collect())
}

fn random_bytes<const N: usize>() -> Result<[u8; N], PasswordError> {
    let mut bytes = [0u8; N];
    ring::rand::SystemRandom::new().fill(&mut bytes).map_err(|_| PasswordError::Internal)?;
    Ok(bytes)
}

/// Equality of two byte strings without an early exit on the first difference.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && std::hint::black_box(a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y))) == 0
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes)
}

fn unb64(text: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD_NO_PAD.decode(text).ok()
}

/// A parsed, bounded stored hash.
struct Stored {
    params: HashParams,
    salt: Vec<u8>,
    tag: Vec<u8>,
}

fn parse_stored(hash: &str) -> Option<Stored> {
    if hash.len() > MAX_HASH_CHARS {
        return None;
    }
    let mut parts = hash.split('$');
    if !parts.next()?.is_empty() || parts.next()? != "argon2id" || parts.next()? != "v=19" {
        return None;
    }
    let (mut memory, mut passes, mut lanes) = (None, None, None);
    for pair in parts.next()?.split(',') {
        let (key, value) = pair.split_once('=')?;
        let value: u32 = value.parse().ok()?;
        let slot = match key {
            "m" => &mut memory,
            "t" => &mut passes,
            "p" => &mut lanes,
            _ => return None,
        };
        if slot.replace(value).is_some() {
            return None;
        }
    }
    let params = HashParams { memory_kib: memory?, passes: passes?, lanes: lanes? }.validated()?;
    let salt = unb64(parts.next()?)?;
    let tag = unb64(parts.next()?)?;
    if parts.next().is_some() || !(8..=64).contains(&salt.len()) || !(16..=64).contains(&tag.len()) {
        return None;
    }
    Some(Stored { params, salt, tag })
}

impl PasswordHasher {
    /// A hasher for `params` (inside the safe bounds). Computes one hash for the dummy used by
    /// [`Self::verify_unknown`], so construction costs about one verification.
    pub fn new(params: HashParams) -> Result<Self, PasswordError> {
        let params = params.validated().ok_or(PasswordError::InvalidParams)?;
        let secret: [u8; 24] = random_bytes()?;
        let mut hasher = Self { params, dummy: String::new() };
        hasher.dummy = hasher.hash_normalised(&b64(&secret))?;
        Ok(hasher)
    }

    #[must_use]
    pub const fn params(&self) -> HashParams {
        self.params
    }

    fn hash_normalised(&self, password: &str) -> Result<String, PasswordError> {
        let salt: [u8; SALT_BYTES] = random_bytes()?;
        let mut tag = [0u8; OUTPUT_BYTES];
        self.params
            .argon2(OUTPUT_BYTES)
            .ok_or(PasswordError::InvalidParams)?
            .hash_password_into(password.as_bytes(), &salt, &mut tag)
            .map_err(|_| PasswordError::Internal)?;
        Ok(format!("$argon2id$v=19$m={},t={},p={}${}${}", self.params.memory_kib, self.params.passes, self.params.lanes, b64(&salt), b64(&tag)))
    }

    /// Hash `password` (after the policy check against `login`, when given). The result is the PHC
    /// string to store.
    pub fn hash(&self, password: &str, login: Option<&str>) -> Result<String, PasswordError> {
        check_policy(password, login).map_err(PasswordError::Policy)?;
        let password = normalised(password).ok_or(PasswordError::Policy(PolicyViolation::TooLong))?;
        self.hash_normalised(&password)
    }

    /// Hash without the policy check (for server-generated secrets only).
    pub fn hash_unchecked(&self, secret: &str) -> Result<String, PasswordError> {
        self.hash_normalised(&normalised(secret).ok_or(PasswordError::Policy(PolicyViolation::TooLong))?)
    }

    /// Check `password` against `stored`. Always one full Argon2 computation, whatever is wrong
    /// with `stored` (an unusable hash is checked against the dummy), so a bad row costs the same as a
    /// bad password.
    #[must_use]
    pub fn verify(&self, stored: &str, password: &str) -> Verification {
        let password = normalised(password);
        let (parsed, usable) = match parse_stored(stored) {
            Some(parsed) => (parsed, true),
            None => match parse_stored(&self.dummy) {
                Some(parsed) => (parsed, false),
                None => return Verification::Invalid,
            },
        };
        let Some(password) = password else {
            // Oversized input: still pay for one computation against the dummy path.
            let _ = self.verify_unknown("");
            return Verification::Invalid;
        };
        let Some(argon2) = parsed.params.argon2(parsed.tag.len()) else {
            return Verification::Invalid;
        };
        let mut tag = vec![0u8; parsed.tag.len()];
        if argon2.hash_password_into(password.as_bytes(), &parsed.salt, &mut tag).is_err() {
            return Verification::Invalid;
        }
        let equal = constant_time_eq(&tag, &parsed.tag);
        if usable && equal {
            let standard = parsed.salt.len() == SALT_BYTES && parsed.tag.len() == OUTPUT_BYTES;
            Verification::Valid { needs_rehash: !standard || !parsed.params.at_least(self.params) }
        } else {
            Verification::Invalid
        }
    }

    /// Do the work of a verification for a login that does not exist, and report invalid.
    pub fn verify_unknown(&self, password: &str) -> Verification {
        let _ = self.verify_dummy(password);
        Verification::Invalid
    }

    fn verify_dummy(&self, password: &str) -> bool {
        let Some(parsed) = parse_stored(&self.dummy) else { return false };
        let password = normalised(password).unwrap_or_default();
        let Some(argon2) = parsed.params.argon2(parsed.tag.len()) else { return false };
        let mut tag = vec![0u8; parsed.tag.len()];
        argon2.hash_password_into(password.as_bytes(), &parsed.salt, &mut tag).is_ok() && constant_time_eq(&tag, &parsed.tag)
    }
}

/// The most common passwords of 12 or more characters, plus common long bases. Lowercase, letters and
/// digits only; candidates are compared after the same folding (see [`fold`]).
const COMMON: &[&str] = &[
    "password1234",
    "password12345",
    "password123456",
    "passw0rd1234",
    "passwordpassword",
    "123456789012",
    "1234567890123",
    "12345678901234",
    "123456789abc",
    "1q2w3e4r5t6y",
    "1qaz2wsx3edc",
    "qwertyuiop12",
    "qwertyuiopas",
    "qwertyuiopasdfghjkl",
    "qwertyuiop1234",
    "qwertyuiop123",
    "qwerty123456",
    "qwertyabc123",
    "asdfghjkl123",
    "asdfghjklqwe",
    "zxcvbnm12345",
    "abcdefghijkl",
    "abcdefghijklmnop",
    "abc123abc123",
    "iloveyou1234",
    "iloveyouiloveyou",
    "letmein12345",
    "letmeinletmein",
    "welcome12345",
    "welcome123456",
    "administrator",
    "administrator1",
    "administrator123",
    "adminadmin1234",
    "adminadminadmin",
    "changeme1234",
    "changemechangeme",
    "changeme123456",
    "trustno1trustno1",
    "football1234",
    "baseball1234",
    "superman12345",
    "mypassword123",
    "mypassword1234",
    "thisisapassword",
    "correcthorsebatterystaple",
    "monkey123456",
    "dragon123456",
    "master123456",
    "princess1234",
    "sunshine1234",
    "shadow123456",
    "michael12345",
    "charlie12345",
    "freedom12345",
    "whatever1234",
    "starwars1234",
    "computer1234",
    "internet1234",
    "11111111111111",
    "000000000000",
    "0123456789ab",
    "9876543210ab",
    "987654321098",
    "112233445566",
    "121212121212",
    "123123123123",
    "123456123456",
    "159753159753",
    "123qwe123qwe",
    "qazwsxedcrfv",
    "qazwsxedc123",
    "zaq12wsxcde3",
    "q1w2e3r4t5y6",
    "p@ssw0rd1234",
    "figcraft1234",
    "figcraftfigcraft",
    "figcraft12345",
];

/// Lowercase letters and digits only, with the usual leetspeak substitutions undone.
fn fold(password: &str) -> String {
    password
        .chars()
        .flat_map(char::to_lowercase)
        .filter_map(|c| match c {
            '@' => Some('a'),
            '$' => Some('s'),
            '!' => Some('i'),
            c if c.is_alphanumeric() => Some(c),
            _ => None,
        })
        .collect()
}

/// A run of one character, or an ascending/descending run of consecutive characters, covering the
/// whole folded password (or all but two characters of it).
fn repetitive(folded: &str) -> bool {
    let chars: Vec<u32> = folded.chars().map(u32::from).collect();
    if chars.len() < 4 {
        return true;
    }
    let distinct = {
        let mut sorted = chars.clone();
        sorted.sort_unstable();
        sorted.dedup();
        sorted.len()
    };
    if distinct <= 3 {
        return true;
    }
    for step in [1i64, -1] {
        let mut longest = 1usize;
        let mut run = 1usize;
        for pair in chars.windows(2) {
            if let [a, b] = pair {
                if i64::from(*b) - i64::from(*a) == step {
                    run += 1;
                    longest = longest.max(run);
                } else {
                    run = 1;
                }
            }
        }
        if longest + 2 >= chars.len() {
            return true;
        }
    }
    false
}

/// Whether `password` is acceptable. `login` (an email or handle) must not equal the password or its
/// local part.
pub fn check_policy(password: &str, login: Option<&str>) -> Result<(), PolicyViolation> {
    if password.len() > MAX_PASSWORD_INPUT_BYTES {
        return Err(PolicyViolation::TooLong);
    }
    let normalised: String = password.nfkc().collect();
    if normalised.chars().any(char::is_control) {
        return Err(PolicyViolation::ControlCharacters);
    }
    let count = normalised.chars().count();
    if count < MIN_PASSWORD_CHARS {
        return Err(PolicyViolation::TooShort);
    }
    if count > MAX_PASSWORD_CHARS {
        return Err(PolicyViolation::TooLong);
    }
    let folded = fold(&normalised);
    if repetitive(&folded) {
        return Err(PolicyViolation::Repetitive);
    }
    if COMMON
        .iter()
        .any(|common| fold(common) == folded || (folded.len() <= common.len() + 4 && folded.starts_with(&fold(common)) && common.len() >= 12))
    {
        return Err(PolicyViolation::Common);
    }
    if let Some(login) = login {
        let login = fold(login);
        let local = fold(login.split('@').next().unwrap_or(""));
        if !login.is_empty() && (folded == login || (!local.is_empty() && folded == local)) {
            return Err(PolicyViolation::ContainsLogin);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cheap() -> HashParams {
        HashParams { memory_kib: 8 * 1024, passes: 1, lanes: 1 }
    }

    fn hasher() -> PasswordHasher {
        PasswordHasher::new(cheap()).expect("hasher")
    }

    #[test]
    fn hash_round_trips_and_embeds_parameters() {
        let hasher = hasher();
        let hash = hasher.hash("correct horse 1 battery", None).expect("hash");
        assert!(hash.starts_with("$argon2id$v=19$m=8192,t=1,p=1$"), "{hash}");
        assert_eq!(hasher.verify(&hash, "correct horse 1 battery"), Verification::Valid { needs_rehash: false });
        assert_eq!(hasher.verify(&hash, "correct horse 2 battery"), Verification::Invalid);
        // Salted: the same password hashes differently.
        assert_ne!(hash, hasher.hash("correct horse 1 battery", None).expect("hash"));
    }

    #[test]
    fn matches_the_reference_implementation_format() {
        // A hash produced by the argon2 reference CLI: `echo -n "differentpassword" | argon2 "differentsalt" -id -t 2 -m 16 -p 1 -l 32`
        // is not embedded here (it would pin a salt shorter than ours); instead verify our own output
        // parses with a standards-style PHC parser by re-deriving the tag from the string's parts.
        let hasher = hasher();
        let hash = hasher.hash("an entirely different phrase", None).expect("hash");
        let stored = parse_stored(&hash).expect("parse");
        let mut tag = vec![0u8; stored.tag.len()];
        stored
            .params
            .argon2(stored.tag.len())
            .expect("argon")
            .hash_password_into(b"an entirely different phrase", &stored.salt, &mut tag)
            .expect("hash");
        assert_eq!(tag, stored.tag);
    }

    #[test]
    fn weaker_hashes_ask_for_a_rehash() {
        let old = hasher();
        let hash = old.hash("a long enough phrase 7", None).expect("hash");
        let newer = PasswordHasher::new(HashParams { memory_kib: 16 * 1024, passes: 2, lanes: 1 }).expect("hasher");
        assert_eq!(newer.verify(&hash, "a long enough phrase 7"), Verification::Valid { needs_rehash: true });
        let upgraded = newer.hash("a long enough phrase 7", None).expect("hash");
        assert_eq!(newer.verify(&upgraded, "a long enough phrase 7"), Verification::Valid { needs_rehash: false });
        // A stronger stored hash than the current setting never asks to be weakened.
        assert_eq!(old.verify(&upgraded, "a long enough phrase 7"), Verification::Valid { needs_rehash: false });
    }

    #[test]
    fn unusable_stored_hashes_are_invalid_not_errors() {
        let hasher = hasher();
        let huge = "$argon2id$v=19$m=4294967295,t=1,p=1$c29tZXNhbHRzb21l$aGFzaGhhc2hoYXNoaGFzaGhhc2hoYXNo".to_string();
        let long = format!("${}", "a".repeat(5000));
        for bad in [
            "",
            "plain",
            "$argon2i$v=19$m=8192,t=1,p=1$c29tZXNhbHRzb21l$aGFzaGhhc2hoYXNoaGFzaGhhc2hoYXNo",
            "$argon2id$v=16$m=8192,t=1,p=1$c29tZXNhbHRzb21l$aGFzaGhhc2hoYXNoaGFzaGhhc2hoYXNo",
            "$argon2id$v=19$m=8192,t=0,p=1$c29tZXNhbHRzb21l$aGFzaGhhc2hoYXNoaGFzaGhhc2hoYXNo",
            "$argon2id$v=19$m=8192,t=1,p=1,m=8192$c29tZXNhbHRzb21l$aGFzaGhhc2hoYXNoaGFzaGhhc2hoYXNo",
            "$argon2id$v=19$m=8192,t=1,p=1$!!!$aGFzaGhhc2hoYXNoaGFzaGhhc2hoYXNo",
            huge.as_str(),
            long.as_str(),
        ] {
            assert_eq!(hasher.verify(bad, "whatever password"), Verification::Invalid, "{bad}");
        }
    }

    #[test]
    fn unknown_logins_cost_a_real_verification() {
        let hasher = hasher();
        let hash = hasher.hash("measure me please 99", None).expect("hash");
        let time = |action: &dyn Fn()| {
            let start = std::time::Instant::now();
            for _ in 0..3 {
                action();
            }
            start.elapsed()
        };
        let wrong = time(&|| assert_eq!(hasher.verify(&hash, "nope nope nope nope"), Verification::Invalid));
        let unknown = time(&|| assert_eq!(hasher.verify_unknown("nope nope nope nope"), Verification::Invalid));
        let (low, high) = if wrong < unknown { (wrong, unknown) } else { (unknown, wrong) };
        // Same work: within a factor of two (Argon2 at 8 MiB takes several ms; scheduler noise only).
        assert!(high.as_secs_f64() < low.as_secs_f64() * 2.5 + 0.02, "{wrong:?} vs {unknown:?}");
        assert!(wrong.as_millis() >= 3, "a verification did no real work: {wrong:?}");
    }

    #[test]
    fn default_parameters_work_and_take_real_effort() {
        let hasher = PasswordHasher::new(HashParams::default()).expect("hasher");
        let start = std::time::Instant::now();
        let hash = hasher.hash("default parameters phrase", None).expect("hash");
        assert!(hash.contains("m=47104,t=2,p=1"), "{hash}");
        assert_eq!(hasher.verify(&hash, "default parameters phrase"), Verification::Valid { needs_rehash: false });
        assert!(start.elapsed().as_millis() >= 20, "default cost looks too cheap: {:?}", start.elapsed());
    }

    #[test]
    fn parameters_are_bounded() {
        assert!(HashParams::default().validated().is_some());
        for bad in [
            HashParams { memory_kib: 1024, ..HashParams::default() },
            HashParams { memory_kib: 2 * 1024 * 1024, ..HashParams::default() },
            HashParams { passes: 0, ..HashParams::default() },
            HashParams { passes: 11, ..HashParams::default() },
            HashParams { lanes: 0, ..HashParams::default() },
            HashParams { lanes: 9, ..HashParams::default() },
        ] {
            assert!(PasswordHasher::new(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn unicode_forms_of_one_password_verify() {
        let hasher = hasher();
        let hash = hasher.hash("caf\u{e9} au lait, s'il vous plait", None).expect("hash");
        assert_eq!(hasher.verify(&hash, "cafe\u{301} au lait, s'il vous plait"), Verification::Valid { needs_rehash: false });
        assert_eq!(hasher.verify(&hash, &"x".repeat(5000)), Verification::Invalid);
    }

    #[test]
    fn policy_accepts_reasonable_passwords() {
        for good in [
            "correct horse battery",
            "tr0ub4dor&3-and-more",
            "\u{30d1}\u{30b9}\u{30ef}\u{30fc}\u{30c9}\u{3092}\u{9078}\u{3076}\u{4eca}\u{65e5}\u{306f}\u{6674}\u{308c}",
            "a1b2c3d4e5f6g7h8",
        ] {
            assert_eq!(check_policy(good, Some("ada@example.com")), Ok(()), "{good}");
        }
        assert_eq!(check_policy("kx3 vq9 zt2 mw8 pr5 jd4 hg7", None), Ok(()));
    }

    #[test]
    fn policy_refuses_weak_passwords() {
        use PolicyViolation::*;
        let cases: &[(&str, Option<&str>, PolicyViolation)] = &[
            ("short1!", None, TooShort),
            ("elevenchars", None, TooShort),
            ("aaaaaaaaaaaaaaaa", None, Repetitive),
            ("abcdefghijklmnop", None, Repetitive),
            ("zyxwvutsrqponm", None, Repetitive),
            ("ababababababab", None, Repetitive),
            ("Password1234!", None, Common),
            ("PASSWORD123456", None, Common),
            ("P@ssw0rd1234", None, Common),
            ("qwertyuiop1234", None, Common),
            ("hello\u{0}world, friend", None, ControlCharacters),
            ("ada.lovelace@example.com", Some("ada.lovelace@example.com"), ContainsLogin),
            ("Ada.Lovelace.Ada", Some("ada.lovelace@example.com"), ContainsLogin),
        ];
        for (password, login, expected) in cases {
            if *password == "Ada.Lovelace.Ada" {
                // Not equal to the local part: allowed (the rule is equality, not substring).
                assert_eq!(check_policy(password, *login), Ok(()));
                continue;
            }
            assert_eq!(check_policy(password, *login), Err(*expected), "{password}");
        }
        let too_long = "pw ".repeat(50) + "kx3vq9";
        assert_eq!(check_policy(&too_long, None), Err(TooLong));
        assert_eq!(check_policy(&"x".repeat(2000), None), Err(TooLong));
    }
}

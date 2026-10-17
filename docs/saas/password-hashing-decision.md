# Password hashing decision

**Status:** Accepted for S6-033 · **Date:** 2026-10-18

FigCraft Cloud offers built-in accounts beside OIDC (S6-032 to S6-038). Passwords are hashed with
**Argon2id** (RFC 9106, version 0x13) through the RustCrypto `argon2` crate, version 0.6.0
(MIT OR Apache-2.0, MSRV 1.85, below the workspace floor).

## Dependency record

| Package | Version | Licence | Why |
|---|---|---|---|
| `argon2` | 0.6.0 | MIT OR Apache-2.0 | The Argon2id primitive. Declared only in `figcraft-cloud-auth` with `default-features = false, features = ["alloc"]`: no RNG, no `rayon`, no key-derivation traits. |
| `blake2` | 0.11.0 | MIT OR Apache-2.0 | Argon2's internal hash (transitive). |
| `password-hash`, `phc` | 0.6.x | MIT OR Apache-2.0 | Transitive requirements of `argon2` 0.6 that cannot be switched off; we do **not** call them. |
| `unicode-normalization` | 0.1.26 | MIT OR Apache-2.0 | NFKC for passwords and NFC for logins; already in the workspace lockfile. |

We call only `Argon2::hash_password_into` and write and parse the PHC string ourselves
(`$argon2id$v=19$m=…,t=…,p=…$salt$hash`, unpadded standard base64). That keeps the surface small and
lets us bound every parameter of a *stored* hash before computing with it. Salts and the dummy secret
come from `ring`'s system generator (already a dependency); no second RNG stack is added.

Alternatives: `bcrypt` and `scrypt` are weaker choices against GPUs and have 72-byte input limits
(bcrypt); `pbkdf2` is not memory hard. A native `libargon2` binding would add C code to the build.

## Parameters

OWASP's Password Storage Cheat Sheet lists Argon2id at m=46 MiB, t=1, p=1 or m=19 MiB, t=2, p=1 (and
equivalent tiers) as the floor. FigCraft defaults to **m = 47104 KiB (46 MiB), t = 2, p = 1**, a 16-byte
salt and a 32-byte output: the stronger memory tier with a second pass. Roughly 50 to 150 ms on a server
core. Operators can change them inside safe bounds: 8 MiB to 1 GiB, 1 to 10 passes, 1 to 8 lanes
(`FIGCRAFT_PASSWORD_MEMORY_KIB`, `FIGCRAFT_PASSWORD_PASSES`, `FIGCRAFT_PASSWORD_LANES`; see
`deployment-operations.md`). Parameters are stored inside every hash, so raising them later affects new
hashes at once and old ones at the next successful sign-in (`needs_rehash`).

## Behaviour that matters

* One Argon2 computation for every verification, including unknown logins (a dummy hash made with the
  current parameters) and unusable stored hashes. Constant-time tag comparison.
* A stored hash is parsed with the same bounds as configuration, so a corrupted row cannot request
  gigabytes of memory.
* Passwords are NFKC-normalised before the policy check and hashing; 12 to 128 characters; Unicode
  allowed; no composition rules; a bundled deny-list of very common long passwords and trivial
  patterns; never equal to the login. The server bounds concurrent hashes (memory use is
  `concurrency × m`).
* No password, hash, salt or tag is ever logged, returned or put in an audit record; `Debug` output of
  hashers and records is redacted.

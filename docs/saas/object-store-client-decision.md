# S3-compatible object client decision

**Status:** Accepted for S3 · **Date:** 2026-10-17 · **Decided by:** product owner, in session

FigCraft Cloud talks to S3-compatible object storage (SeaweedFS in the local Compose stack, any
S3-compatible service in production) through `figcraft-cloud-storage::objects`. Requests are
signed with AWS Signature Version 4 by [`rusty-s3`](https://crates.io/crates/rusty-s3) 0.10 and sent
with [`reqwest`](https://crates.io/crates/reqwest) 0.13.

## Why this shape

- `rusty-s3` is sans-I/O: it builds presigned URLs for each action and performs no network
  access. The same signer produces the short-lived presigned requests that browsers use for
  uploads and downloads (S3-004, S3-006) and the requests the server sends itself, so there is one
  signing implementation to test.
- `reqwest` is built with `default-features = false` and `rustls-no-provider`. The client is
  handed a preconfigured Rustls configuration using the `ring` provider and the compiled-in
  Mozilla root store (`webpki-roots`), matching the workspace's existing TLS choice (`sqlx`'s
  `tls-rustls-ring-webpki`, `figcraft-collab-net`). No OpenSSL, no `aws-lc-rs`, and no OS
  certificate store is used at runtime.
- Redirects and proxies are disabled so a signed request reaches only the configured endpoint;
  every request has connect and total timeouts. Errors are fixed categories that never echo a
  URL, signature or response body.
- Bucket administration that `rusty-s3` has no action for (versioning, lifecycle; S3-014) is
  signed through the same `S3Action` path with custom query parameters rather than a second
  client.

The dependency is confined to the cloud-storage adapter and is not part of the editor or WASM
graph. It adds 28 crates to the lockfile.

## Licences and toolchain

| Crate | Licence | Notes |
|---|---|---|
| `rusty-s3` 0.10.2 | BSD-2-Clause | Permissive; binary distributions must reproduce its copyright notice and licence text with other third-party notices |
| `reqwest` 0.13.5 | MIT OR Apache-2.0 | |
| `hyper`, `hyper-util`, `hyper-rustls`, `tokio-rustls`, `tower*`, `instant-xml`, `xmlparser`, `ipnet` | MIT and/or Apache-2.0 | Transitive |
| `jiff` | Unlicense OR MIT | Transitive (signing timestamps) |
| `webpki-roots`, `webpki-root-certs` | CDLA-Permissive-2.0 | Already used by `figcraft-collab-net` |
| `rustls-platform-verifier` and its platform crates | MIT OR Apache-2.0 | Compiled in by the `rustls-no-provider` feature but not used: the client installs its own root store |

Both direct crates declare Rust 1.85 as their minimum, below the workspace's 1.95.

## Alternatives considered

- **`aws-sdk-s3`:** the most complete S3 semantics, but a much larger dependency graph, longer
  builds and more disk, and an `aws-lc-rs` default that would need overriding.
- **`object_store`:** a clean put/get/list API with presigning and test backends, but no bucket
  administration, so versioning and lifecycle would still need hand-signed requests.
- **A hand-written SigV4 and HTTP/1.1 client:** no new large dependencies, but FigCraft would own
  an HTTP client (chunked bodies, keep-alive, TLS), the riskiest piece to get right.

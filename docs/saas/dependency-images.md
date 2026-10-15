# SaaS dependency image review

> **Review date:** 2026-10-16 · **Decision:** D-001 partially resolved; object storage remains open

The Compose development template pins two reviewed, multi-platform Docker Official Images. A
tag names the human-readable release and the digest fixes the exact bytes. Re-review upstream
security notices and replace both tag and digest deliberately; never update the tag without the
digest or silently follow a moving major-version tag. These pins are not proof that the entire
SaaS stack works or a substitute for deployment-specific vulnerability review.

| Service | Selected image | Reason |
|---|---|---|
| PostgreSQL | `postgres:18.6-bookworm@sha256:afc7e2d441324c0388fa80c3d24f733b4194a4eb7f47dd8ee2b08eb1a24a647c` | [18.6 is supported](https://www.postgresql.org/support/versioning/); the [official image](https://github.com/docker-library/official-images/blob/master/library/postgres) publishes the tag. |
| Redis | `redis:7.2.16-bookworm@sha256:46f15cf64fdef63e483dcda99e74e3d51c017b1175547b6bb874152ee2391aad` | [Redis Open Source 7.2 remains supported](https://redis.io/docs/latest/operate/oss_and_stack/install/version-mgmt/) and [BSD-3-Clause licensed](https://redis.io/legal/licenses/); the [official image](https://github.com/docker-library/official-images/blob/master/library/redis) publishes the tag. The separate *Redis Software* 7.2 end-of-life date does not describe this image. |

The OCI index digests above were read with `docker buildx imagetools inspect` and verified by
pulling those exact references. Inspection of the pulled PostgreSQL image confirmed
`PGDATA=/var/lib/postgresql/18/docker` and the declared volume `/var/lib/postgresql`; the
[official image documentation](https://hub.docker.com/_/postgres) requires mounting that volume
target for major 18. Inspection of the Redis image confirmed `docker-entrypoint.sh`, the
`redis-server` command and `/data` volume. This validates image metadata, **not** the Compose
health checks, data durability or a running SaaS stack.

## Open object-storage decision

The [upstream MinIO server repository](https://github.com/minio/minio) and
[MinIO Client repository](https://github.com/minio/mc) are archived. Historical binaries are
explicitly described by upstream as unmaintained, and the server is AGPLv3. Therefore no MinIO
image is pinned in `env.example`, and Compose must continue to refuse deployment without an
explicit object-storage and initializer choice. Do not mistake the newest historical tag for a
supported release.

Before closing D-001, select a maintained S3-compatible service whose image and bundled assets
meet this repository's license policy, then verify the exact release, digest, secret handling,
bucket creation, versioning, lifecycle behavior and persistence in Compose. The accepted
[SAAS-ADR-005](README.md#architecture-decisions) allows S3-compatible storage, but a provider
change needs the Compose configuration and all MinIO-specific operational documents updated in
the same implementation arc. A managed S3 service is another deployment option, but it cannot
validate the current seven-service local Compose stack by itself.

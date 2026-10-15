# SaaS dependency image review

> **Review date:** 2026-10-16 · **Decision:** D-001 local image release policy selected

The Compose development template pins two reviewed, multi-platform Docker Official Images and a
maintained S3-compatible object-store image. A tag names the human-readable release and the digest
fixes the exact bytes. Re-review upstream
security notices and replace each tag and digest pair deliberately; never update a tag without its
digest or silently follow a moving major-version tag. These pins are not proof that the entire
SaaS stack works or a substitute for deployment-specific vulnerability review.

| Service | Selected image | Reason |
|---|---|---|
| PostgreSQL | `postgres:18.6-bookworm@sha256:afc7e2d441324c0388fa80c3d24f733b4194a4eb7f47dd8ee2b08eb1a24a647c` | [18.6 is supported](https://www.postgresql.org/support/versioning/); the [official image](https://github.com/docker-library/official-images/blob/master/library/postgres) publishes the tag. |
| Redis | `redis:7.2.16-bookworm@sha256:46f15cf64fdef63e483dcda99e74e3d51c017b1175547b6bb874152ee2391aad` | [Redis Open Source 7.2 remains supported](https://redis.io/docs/latest/operate/oss_and_stack/install/version-mgmt/) and [BSD-3-Clause licensed](https://redis.io/legal/licenses/); the [official image](https://github.com/docker-library/official-images/blob/master/library/redis) publishes the tag. The separate *Redis Software* 7.2 end-of-life date does not describe this image. |
| SeaweedFS | `chrislusf/seaweedfs:4.48@sha256:4e61d15fd35994cb1e43e1e553dff106794841fd9a99ade2fc8c8bfce4d7872d` | [4.48 is an active release](https://github.com/seaweedfs/seaweedfs/releases/tag/4.48), [Apache-2.0 licensed](https://github.com/seaweedfs/seaweedfs/blob/4.48/LICENSE), with [S3 bucket-versioning APIs](https://github.com/seaweedfs/seaweedfs/wiki/Amazon-S3-API). The same image runs the one-shot `weed shell` versioning initializer. |

The OCI index digests above were read with `docker buildx imagetools inspect` and verified by
pulling those exact references. Inspection of the pulled PostgreSQL image confirmed
`PGDATA=/var/lib/postgresql/18/docker` and the declared volume `/var/lib/postgresql`; the
[official image documentation](https://hub.docker.com/_/postgres) requires mounting that volume
target for major 18. Inspection of the Redis image confirmed `docker-entrypoint.sh`, the
`redis-server` command and `/data` volume. Those inspections alone validated image metadata, not
health checks, data durability or a running SaaS stack; runtime evidence follows.

## Object-store replacement and validation

The [upstream MinIO server repository](https://github.com/minio/minio) and
[MinIO Client repository](https://github.com/minio/mc) are archived. Historical binaries are
explicitly described by upstream as unmaintained, and the server is AGPLv3. SeaweedFS replaces
MinIO in the local Compose scaffold under the S3-compatible option in
[SAAS-ADR-005](README.md#architecture-decisions). No MinIO binary or image is shipped by this
deployment. Do not mistake the newest historical MinIO tag for a supported release.

The exact SeaweedFS image was pulled and its `mini` command tested with a synthetic S3 identity.
The development stack's PostgreSQL and Redis health checks passed. SeaweedFS `/readyz` passed, its
one-shot initializer enabled versioning and succeeded on a second run, and versioning survived a
container restart. A PostgreSQL marker row and an authenticated Redis marker survived restarts.
Through the signed S3 API, `GetBucketVersioning` returned `Enabled`; `PutObject` returned a
version ID, and `HeadObject` returned the same version ID after restarting SeaweedFS. The test
used a disposable Compose project and synthetic credentials, not customer data.

This closes the **local image selection** part of D-001. It does not certify hosted production:
the single-node SeaweedFS `mini` topology exposes internal master/filer/volume/admin endpoints on
the private Compose network, and needs a deployment-specific network/security review before
production. Lifecycle-policy provisioning and backup/restore remain S3/S8 tasks; neither is
claimed as validated here. The full API/web/WebSocket smoke test remains S1-018 because the cloud
application routes are not implemented.

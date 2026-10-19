#!/usr/bin/env bash
# Exercise the current S1 Compose contract without claiming the unfinished SaaS app is ready.
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_dir=$(cd -- "$script_dir/../.." && pwd)
test_dir=$(mktemp -d /tmp/figcraft-s1-preflight.XXXXXX)
project="figcraft-s1-preflight-$(openssl rand -hex 6)"
compose=(docker compose --project-name "$project" --env-file "$script_dir/env.example" --file "$script_dir/compose.yaml" --file "$test_dir/override.yaml")
role_containers=()

cleanup() {
  for container in "${role_containers[@]}"; do
    docker rm --force "$container" >/dev/null 2>&1 || true
  done
  if ! "${compose[@]}" down --volumes --remove-orphans >/dev/null 2>&1; then
    printf 'Warning: could not remove disposable Compose project %s; inspect it manually\n' "$project" >&2
  fi
  case "$test_dir" in
    /tmp/figcraft-s1-preflight.*) rm -rf -- "$test_dir" ;;
    *) printf 'Refusing to remove unexpected test directory: %s\n' "$test_dir" >&2 ;;
  esac
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
umask 077

postgres_password=$(openssl rand -hex 24)
migrator_password=$(openssl rand -hex 24)
runtime_password=$(openssl rand -hex 24)
redis_password=$(openssl rand -hex 24)
s3_access_key="FCsmoke$(openssl rand -hex 12)"
s3_secret_key=$(openssl rand -hex 24)
printf 'postgres://figcraft:%s@postgres:5432/figcraft\n' "$postgres_password" > "$test_dir/admin_database_url"
printf 'postgres://figcraft_migrator:%s@postgres:5432/figcraft\n' "$migrator_password" > "$test_dir/migration_database_url"
printf 'postgres://figcraft_runtime:%s@postgres:5432/figcraft\n' "$runtime_password" > "$test_dir/database_url"
printf '%s\n' "$migrator_password" > "$test_dir/migrator_password"
printf '%s\n' "$runtime_password" > "$test_dir/runtime_password"
printf 'redis://:%s@redis:6379/\n' "$redis_password" > "$test_dir/redis_url"
printf '%s\n' "$postgres_password" > "$test_dir/postgres_password"
printf '%s' "$redis_password" > "$test_dir/redis_password"
printf 'requirepass %s\n' "$redis_password" > "$test_dir/redis.conf"
printf '%s\n' "$s3_access_key" > "$test_dir/s3_access_key"
printf '%s\n' "$s3_secret_key" > "$test_dir/s3_secret_key"
printf '%s\n' "$(openssl rand -hex 24)" > "$test_dir/oidc_client_secret"
printf '%s\n' "$(openssl rand -hex 32)" > "$test_dir/session_signing_key"
printf '{"identities":[{"name":"smoke","credentials":[{"accessKey":"%s","secretKey":"%s"}],"actions":["Admin","Read","Write","List"]}]}\n' \
  "$s3_access_key" "$s3_secret_key" > "$test_dir/seaweedfs-s3-config.json"

# Docker Compose (outside Swarm) bind-mounts a `secrets.*.file` source preserving its exact host
# permission bits, rather than normalizing them the way a Swarm-deployed secret would. The `umask
# 077` above otherwise leaves these files unreadable by the distroless image's nonroot container
# user (UID 65532), so every secret-consuming service would fail to start. `$test_dir` itself stays
# private (mktemp -d created it 0700), so widening only these disposable, random-valued files is safe.
chmod 644 "$test_dir"/*

{
  printf 'services:\n  gateway:\n    ports:\n      - "127.0.0.1::8080"\n'
  printf '  postgres:\n    ports:\n      - "127.0.0.1::5432"\n'
  printf '  seaweedfs:\n    ports:\n      - "127.0.0.1::8333"\n'
  printf 'secrets:\n'
  for secret in admin_database_url migration_database_url database_url migrator_password runtime_password redis_url postgres_password redis_password redis_conf s3_access_key s3_secret_key oidc_client_secret session_signing_key seaweedfs_s3_config; do
    filename=$secret
    case "$secret" in
      redis_conf) filename=redis.conf ;;
      seaweedfs_s3_config) filename=seaweedfs-s3-config.json ;;
    esac
    printf '  %s:\n    file: %s/%s\n' "$secret" "$test_dir" "$filename"
  done
} > "$test_dir/override.yaml"

cd -- "$repo_dir"
"${compose[@]}" config --quiet
printf 'Starting disposable S1 preflight project %s\n' "$project"
"${compose[@]}" up --detach --build gateway

health_status() {
  local service=$1
  local container
  container=$("${compose[@]}" ps --all --quiet "$service")
  test -n "$container"
  docker inspect --format '{{.State.Health.Status}}' "$container"
}

wait_healthy() {
  local service=$1
  local status=starting
  for _ in {1..30}; do
    status=$(health_status "$service")
    test "$status" = healthy && return 0
    if test "$status" = unhealthy; then
      break
    fi
    sleep 2
  done
  printf '%s should be healthy, got %s\n' "$service" "$status" >&2
  return 1
}

for service in postgres redis seaweedfs web cloud gateway; do
  if ! wait_healthy "$service"; then
    exit 1
  fi
done

init_container=$("${compose[@]}" ps --all --quiet seaweedfs-init)
test -n "$init_container"
if test "$(docker inspect --format '{{.State.ExitCode}}' "$init_container")" != 0; then
  printf 'Object store initialization (bucket, versioning, lifecycle) failed\n' >&2
  exit 1
fi

migration_container=$("${compose[@]}" ps --all --quiet migrate)
if test -z "$migration_container"; then
  printf 'Compose migration job should exist\n' >&2
  exit 1
fi
for service in role-provision role-grants; do
  role_job=$("${compose[@]}" ps --all --quiet "$service")
  if test -z "$role_job" || test "$(docker inspect --format '{{.State.ExitCode}}' "$role_job")" != 0; then
    printf 'Database role job %s should complete successfully\n' "$service" >&2
    exit 1
  fi
done
if test "$(docker inspect --format '{{.State.ExitCode}}' "$migration_container")" != 0; then
  printf 'Compose migration job should complete successfully before cloud startup\n' >&2
  docker logs "$migration_container" >&2 || true
  exit 1
fi

migration_ledger() {
  "${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -Atqc \
    "SELECT to_regclass('public._sqlx_migrations') IS NOT NULL"
}
if test "$(migration_ledger)" != t; then
  printf 'Explicit Compose migration should create the SQLx migration ledger\n' >&2
  exit 1
fi
schema_version=$("${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -Atqc \
  "SELECT MAX(version) FROM public._sqlx_migrations WHERE success = TRUE")
if test "$schema_version" != 20261018000033; then
  printf 'Expected the S5 room lease migration, got schema version %s\n' "$schema_version" >&2
  exit 1
fi
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/schema-preflight.sql" > "$test_dir/schema-preflight.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  -c 'GRANT CREATE ON SCHEMA public TO figcraft_runtime; GRANT DELETE ON public.audit_events TO figcraft_runtime; GRANT INSERT ON public._sqlx_migrations TO figcraft_runtime' \
  > "$test_dir/stale-grants.log"
"${compose[@]}" run --rm --no-deps role-grants > "$test_dir/reconcile-grants.log" 2>&1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/role-preflight.sql" > "$test_dir/role-preflight.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/tenant-context-preflight.sql" > "$test_dir/tenant-context-preflight.log"
postgres_binding=$("${compose[@]}" port postgres 5432)
postgres_port=${postgres_binding##*:}
if [[ ! "$postgres_port" =~ ^[0-9]+$ ]]; then
  printf 'Expected a disposable loopback PostgreSQL port\n' >&2
  exit 1
fi
printf 'postgres://figcraft_runtime:%s@127.0.0.1:%s/figcraft\n' "$runtime_password" "$postgres_port" > "$test_dir/host_database_url"
printf 'postgres://figcraft:%s@127.0.0.1:%s/figcraft\n' "$postgres_password" "$postgres_port" > "$test_dir/host_admin_database_url"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" \
  cargo test --locked -p figcraft-cloud-storage --test tenant_transaction -- --nocapture --test-threads=1
seaweedfs_binding=$("${compose[@]}" port seaweedfs 8333)
seaweedfs_port=${seaweedfs_binding##*:}
if [[ ! "$seaweedfs_port" =~ ^[0-9]+$ ]]; then
  printf 'Expected a disposable loopback SeaweedFS S3 port\n' >&2
  exit 1
fi
mkdir -p "$test_dir/s3"
printf 'http://127.0.0.1:%s\n' "$seaweedfs_port" > "$test_dir/s3/s3_endpoint"
cp "$test_dir/s3_access_key" "$test_dir/s3/s3_access_key"
cp "$test_dir/s3_secret_key" "$test_dir/s3/s3_secret_key"
FIGCRAFT_TEST_S3_DIR="$test_dir/s3" \
  cargo test --locked -p figcraft-cloud-storage --test object_store -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/object-repository-preflight.sql" > "$test_dir/object-repository-preflight.log"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" FIGCRAFT_TEST_S3_DIR="$test_dir/s3" \
  cargo test --locked -p figcraft-cloud-storage --test object_repository -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/object-repository-cleanup.sql" > "$test_dir/object-repository-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/quarantine-preflight.sql" > "$test_dir/quarantine-preflight.log"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE="$test_dir/host_admin_database_url" \
  cargo test --locked -p figcraft-cloud-relay --test quarantine -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/quarantine-cleanup.sql" > "$test_dir/quarantine-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/orphan-sweep-preflight.sql" > "$test_dir/orphan-sweep-preflight.log"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" FIGCRAFT_TEST_S3_DIR="$test_dir/s3" \
  cargo test --locked -p figcraft-cloud-storage --lib orphan_sweep -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/orphan-sweep-cleanup.sql" > "$test_dir/orphan-sweep-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/legacy-import-preflight.sql" > "$test_dir/legacy-import-preflight.log"
FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE="$test_dir/host_admin_database_url" FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" FIGCRAFT_TEST_S3_DIR="$test_dir/s3" \
  cargo test --locked -p figcraft-cloud-storage --test legacy_import -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/legacy-import-cleanup.sql" > "$test_dir/legacy-import-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/backup-restore-preflight.sql" > "$test_dir/backup-restore-preflight.log"
FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE="$test_dir/host_admin_database_url" FIGCRAFT_TEST_S3_DIR="$test_dir/s3" \
  cargo test --locked -p figcraft-cloud-storage --test backup_restore -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/backup-restore-cleanup.sql" > "$test_dir/backup-restore-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/document-snapshot-preflight.sql" > "$test_dir/document-snapshot-preflight.log"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" FIGCRAFT_TEST_S3_DIR="$test_dir/s3" \
  cargo test --locked -p figcraft-cloud-storage --test document_snapshots -- --nocapture --test-threads=1
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" FIGCRAFT_TEST_S3_DIR="$test_dir/s3" \
  cargo test --locked -p figcraft-cloud-relay --test snapshot_restore -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/document-snapshot-cleanup.sql" > "$test_dir/document-snapshot-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/asset-preflight.sql" > "$test_dir/asset-preflight.log"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" FIGCRAFT_TEST_S3_DIR="$test_dir/s3" \
  cargo test --locked -p figcraft-cloud-storage --test assets -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/asset-cleanup.sql" > "$test_dir/asset-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/library-preflight.sql" > "$test_dir/library-preflight.log"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" FIGCRAFT_TEST_S3_DIR="$test_dir/s3" \
  cargo test --locked -p figcraft-cloud-storage --test libraries -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/library-cleanup.sql" > "$test_dir/library-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/plugin-store-preflight.sql" > "$test_dir/plugin-store-preflight.log"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" FIGCRAFT_TEST_S3_DIR="$test_dir/s3" \
  cargo test --locked -p figcraft-cloud-storage --test plugin_store -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/plugin-store-cleanup.sql" > "$test_dir/plugin-store-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/export-preflight.sql" > "$test_dir/export-preflight.log"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" FIGCRAFT_TEST_S3_DIR="$test_dir/s3" \
  cargo test --locked -p figcraft-cloud-storage --test exports -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/export-cleanup.sql" > "$test_dir/export-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/garbage-collection-preflight.sql" > "$test_dir/garbage-collection-preflight.log"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" FIGCRAFT_TEST_S3_DIR="$test_dir/s3" \
  cargo test --locked -p figcraft-cloud-storage --test garbage_collection -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/garbage-collection-cleanup.sql" > "$test_dir/garbage-collection-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/reconciliation-preflight.sql" > "$test_dir/reconciliation-preflight.log"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" FIGCRAFT_TEST_S3_DIR="$test_dir/s3" \
  cargo test --locked -p figcraft-cloud-storage --test reconciliation -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/reconciliation-cleanup.sql" > "$test_dir/reconciliation-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/upload-reservation-preflight.sql" > "$test_dir/upload-reservation-preflight.log"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" FIGCRAFT_TEST_S3_DIR="$test_dir/s3" \
  cargo test --locked -p figcraft-cloud-storage --test upload_reservations -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/upload-reservation-cleanup.sql" > "$test_dir/upload-reservation-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/presigned-download-preflight.sql" > "$test_dir/presigned-download-preflight.log"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" FIGCRAFT_TEST_S3_DIR="$test_dir/s3" \
  cargo test --locked -p figcraft-cloud-storage --test presigned_downloads -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/presigned-download-cleanup.sql" > "$test_dir/presigned-download-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/object-reference-preflight.sql" > "$test_dir/object-reference-preflight.log"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" \
  cargo test --locked -p figcraft-cloud-storage --test object_references -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/object-reference-cleanup.sql" > "$test_dir/object-reference-cleanup.log"
FIGCRAFT_TEST_S3_DIR="$test_dir/s3" \
  cargo test --locked -p figcraft-cloud-storage --test bucket_admin -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  -c 'DROP TABLE public.tenant_context_probe' > "$test_dir/tenant-context-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/metadata-repository-preflight.sql" > "$test_dir/metadata-repository-preflight.log"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" \
  cargo test --locked -p figcraft-cloud-storage --test metadata_repositories -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/metadata-repository-cleanup.sql" > "$test_dir/metadata-repository-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/document-repository-preflight.sql" > "$test_dir/document-repository-preflight.log"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" \
  cargo test --locked -p figcraft-cloud-storage --test document_repositories -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/document-repository-cleanup.sql" > "$test_dir/document-repository-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/external-mapping-repository-preflight.sql" > "$test_dir/external-mapping-repository-preflight.log"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" \
  cargo test --locked -p figcraft-cloud-storage --test external_mapping_repository -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/external-mapping-repository-cleanup.sql" > "$test_dir/external-mapping-repository-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/idempotency-repository-preflight.sql" > "$test_dir/idempotency-repository-preflight.log"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" \
  cargo test --locked -p figcraft-cloud-storage --test idempotency_repository -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/idempotency-repository-cleanup.sql" > "$test_dir/idempotency-repository-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/audit-repository-preflight.sql" > "$test_dir/audit-repository-preflight.log"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" \
  cargo test --locked -p figcraft-cloud-storage --test audit_repository -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/audit-repository-cleanup.sql" > "$test_dir/audit-repository-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/snapshot-repository-preflight.sql" > "$test_dir/snapshot-repository-preflight.log"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" \
  cargo test --locked -p figcraft-cloud-storage --test snapshot_repository -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/snapshot-repository-cleanup.sql" > "$test_dir/snapshot-repository-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/event-repository-preflight.sql" > "$test_dir/event-repository-preflight.log"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" \
  cargo test --locked -p figcraft-cloud-storage --test event_repository -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/event-repository-cleanup.sql" > "$test_dir/event-repository-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/event-replay-preflight.sql" > "$test_dir/event-replay-preflight.log"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" \
  cargo test --locked -p figcraft-cloud-storage --test event_replay -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/event-replay-cleanup.sql" > "$test_dir/event-replay-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/concurrency-preflight.sql" > "$test_dir/concurrency-preflight.log"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" \
  cargo test --locked -p figcraft-cloud-storage --test event_concurrency -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/concurrency-cleanup.sql" > "$test_dir/concurrency-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/durable-room-preflight.sql" > "$test_dir/durable-room-preflight.log"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" \
  cargo test --locked -p figcraft-cloud-relay --test durable_room -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/durable-room-cleanup.sql" > "$test_dir/durable-room-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/failure-injection-preflight.sql" > "$test_dir/failure-injection-preflight.log"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" \
  cargo test --locked -p figcraft-cloud-storage --test failure_injection -- --nocapture --test-threads=1
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" \
  cargo test --locked -p figcraft-cloud-relay --test room_failure_injection -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/failure-injection-cleanup.sql" > "$test_dir/failure-injection-cleanup.log"
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/s2-gate-preflight.sql" > "$test_dir/s2-gate-preflight.log"
FIGCRAFT_TEST_DATABASE_URL_FILE="$test_dir/host_database_url" \
  cargo test --locked -p figcraft-cloud-relay --test s2_gate -- --nocapture --test-threads=1
"${compose[@]}" exec -T postgres psql -U figcraft -d figcraft -q -v ON_ERROR_STOP=1 \
  < "$script_dir/s2-gate-cleanup.sql" > "$test_dir/s2-gate-cleanup.log"
FIGCRAFT_TEST_ADMIN_DATABASE_URL_FILE="$test_dir/host_admin_database_url" \
  cargo test --locked -p figcraft-cloud-storage --test migration_upgrade -- --nocapture --test-threads=1
"${compose[@]}" run --rm --no-deps migrate > "$test_dir/migration-a.log" 2>&1 &
migration_a=$!
"${compose[@]}" run --rm --no-deps migrate > "$test_dir/migration-b.log" 2>&1 &
migration_b=$!
migrations_ok=true
if ! wait "$migration_a"; then
  migrations_ok=false
fi
if ! wait "$migration_b"; then
  migrations_ok=false
fi
if test "$migrations_ok" != true; then
  cat "$test_dir/migration-a.log" "$test_dir/migration-b.log" >&2
  printf 'Concurrent migration commands should serialize and succeed\n' >&2
  exit 1
fi
if test "$(migration_ledger)" != t; then
  printf 'Explicit migration command should create the SQLx migration ledger\n' >&2
  exit 1
fi

gateway_container=$("${compose[@]}" ps --all --quiet gateway)
test -n "$gateway_container"
if test "$(docker inspect --format '{{.State.Running}}' "$gateway_container")" != true; then
  printf 'Gateway should run after cloud transport readiness succeeds\n' >&2
  exit 1
fi

host_port=$(docker inspect --format '{{(index (index .NetworkSettings.Ports "8080/tcp") 0).HostPort}}' "$gateway_container")
base_url="http://127.0.0.1:$host_port"
curl --fail --silent --show-error --output "$test_dir/web.html" "$base_url/"
grep -Fq '<title>FigCraft</title>' "$test_dir/web.html"
curl --fail --silent --show-error --output "$test_dir/api.json" "$base_url/api/v1/healthz"
grep -Fq '"status":"transport_ready"' "$test_dir/api.json"
curl --silent --show-error --http1.1 --output "$test_dir/ws-body" --dump-header "$test_dir/ws-headers" --max-time 5 \
  --header 'Connection: keep-alive, Upgrade' --header 'Upgrade: websocket' --header 'Sec-WebSocket-Version: 13' \
  --header 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' "$base_url/ws/v1/health"
grep -Fq '101 Switching Protocols' "$test_dir/ws-headers"

if "${compose[@]}" run --rm --no-deps -e FIGCRAFT_ENV=production -e FIGCRAFT_PUBLIC_URL=http://127.0.0.1:8080 cloud --role=api \
  > "$test_dir/invalid-config.log" 2>&1; then
  printf 'Unsafe production configuration should be rejected\n' >&2
  exit 1
fi
grep -Fq 'invalid cloud configuration' "$test_dir/invalid-config.log"

cloud_container=$("${compose[@]}" ps --all --quiet cloud)
cloud_image=$(docker inspect --format '{{.Image}}' "$cloud_container")
for role in all api relay worker; do
  container_name="$project-role-$role"
  role_container=$("${compose[@]}" run --detach --no-deps --name "$container_name" cloud "--role=$role")
  role_containers+=("$role_container")
  running=false
  for _ in {1..10}; do
    if test "$(docker inspect --format '{{.State.Running}}' "$role_container")" = true; then
      running=true
      break
    fi
    sleep 1
  done
  if test "$running" != true; then
    printf 'Cloud role %s did not stay running\n' "$role" >&2
    docker logs "$role_container" >&2 || true
    exit 1
  fi
  if test "$(docker inspect --format '{{.Image}}' "$role_container")" != "$cloud_image"; then
    printf 'Cloud role %s did not use the Compose cloud image\n' "$role" >&2
    exit 1
  fi
  if ! docker logs "$role_container" 2>&1 | grep -Fq "for role '$role'"; then
    printf 'Cloud image did not report role %s at startup\n' "$role" >&2
    exit 1
  fi
  docker rm --force "$role_container" >/dev/null
done

printf 'S1/S2 smoke passed: database roles and grants; synthetic tenant-context, metadata, document lifecycle, external mapping, idempotency-key and audit repository isolation; snapshot metadata transitions; event append sequencing and fencing; bounded replay; sequence, fencing and idempotency races; commit-before-broadcast rooms; commit, connection-loss and retry failure injection; the S2 create/edit/restart/restore gate; upgrade from the previous schema; schema constraints; concurrent migrations; cloud routes; unsafe-config rejection; object-store connectivity, credential and bucket checks; object read-back verification; upload reservations and validated promotion; authorized presigned downloads; references and retention holds; bucket versioning and lifecycle initialization; snapshot store and recovery; assets, libraries, plugins, exports and thumbnails; garbage collection; reconciliation; repeatable legacy import and cutover verification; backup and restore.\n'

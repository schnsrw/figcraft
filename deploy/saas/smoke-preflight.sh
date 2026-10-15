#!/usr/bin/env bash
# Exercise the current S1 Compose contract without claiming the unfinished SaaS app is ready.
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_dir=$(cd -- "$script_dir/../.." && pwd)
test_dir=$(mktemp -d /tmp/figcraft-s1-preflight.XXXXXX)
project="figcraft-s1-preflight-$(openssl rand -hex 6)"
compose=(docker compose --project-name "$project" --env-file "$script_dir/env.example" --file "$script_dir/compose.yaml" --file "$test_dir/override.yaml")

cleanup() {
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
redis_password=$(openssl rand -hex 24)
s3_access_key="FCsmoke$(openssl rand -hex 12)"
s3_secret_key=$(openssl rand -hex 24)
printf 'postgres://figcraft:%s@postgres:5432/figcraft\n' "$postgres_password" > "$test_dir/database_url"
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

{
  printf 'secrets:\n'
  for secret in database_url redis_url postgres_password redis_password redis_conf s3_access_key s3_secret_key oidc_client_secret session_signing_key seaweedfs_s3_config; do
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
if "${compose[@]}" up --detach --build gateway; then
  printf 'Unexpectedly reached a healthy gateway before cloud application routes exist\n' >&2
  exit 1
fi

health_status() {
  local service=$1
  local container
  container=$("${compose[@]}" ps --all --quiet "$service")
  test -n "$container"
  docker inspect --format '{{.State.Health.Status}}' "$container"
}

for service in postgres redis seaweedfs web; do
  status=$(health_status "$service")
  if test "$status" != healthy; then
    printf '%s should be healthy, got %s\n' "$service" "$status" >&2
    exit 1
  fi
done

init_container=$("${compose[@]}" ps --all --quiet seaweedfs-init)
test -n "$init_container"
if test "$(docker inspect --format '{{.State.ExitCode}}' "$init_container")" != 0; then
  printf 'Bucket versioning initialization failed\n' >&2
  exit 1
fi
if test "$(health_status cloud)" != unhealthy; then
  printf 'Cloud readiness must fail until application routes exist\n' >&2
  exit 1
fi
gateway_container=$("${compose[@]}" ps --all --quiet gateway)
if test -n "$gateway_container" && test "$(docker inspect --format '{{.State.Running}}' "$gateway_container")" = true; then
  printf 'Gateway must not run while cloud readiness fails\n' >&2
  exit 1
fi

printf 'S1 preflight passed: dependencies healthy, cloud unready, gateway closed. S1-018 remains open.\n'

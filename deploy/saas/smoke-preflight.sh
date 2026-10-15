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
  printf 'services:\n  gateway:\n    ports:\n      - "127.0.0.1::8080"\n'
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
  printf 'Bucket versioning initialization failed\n' >&2
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

printf 'S1 smoke passed: all roles start from one image; Compose, web, API and WebSocket routes work; unsafe production config is rejected.\n'

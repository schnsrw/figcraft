#!/usr/bin/env bash
# Back up a single-node FigCraft Cloud stack: the PostgreSQL database, the object store and the
# secrets. Run it from anywhere on the host that runs the Compose stack.
#
#   deploy/saas/backup.sh [backup-directory]
#
# The cloud service and the object store are stopped for the few seconds the copy takes, so the
# database and the objects belong to the same moment; editors reconnect by themselves afterwards.
# The backup holds the secrets (the session signing key seals webhook secrets and signs sessions):
# store it like a password vault, never beside the server it protects.
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
env_file=${FIGCRAFT_ENV_FILE:-}
if [ -z "$env_file" ]; then
  if [ -f "$script_dir/.env" ]; then env_file="$script_dir/.env"; else env_file="$script_dir/env.example"; fi
fi
project=${FIGCRAFT_COMPOSE_PROJECT:-figcraft-saas}
compose=(docker compose --project-name "$project" --env-file "$env_file" --file "$script_dir/compose.yaml")
# Optional extra Compose file (for example one that points the secrets somewhere else).
if [ -n "${FIGCRAFT_COMPOSE_OVERRIDE:-}" ]; then compose+=(--file "$FIGCRAFT_COMPOSE_OVERRIDE"); fi
out=${1:-"$PWD/figcraft-backup-$(date -u +%Y%m%dT%H%M%SZ)"}

if [ -e "$out" ]; then
  printf 'Refusing to write into an existing path: %s\n' "$out" >&2
  exit 1
fi
umask 077
mkdir -p "$out"

volume() {
  docker volume ls --quiet \
    --filter "label=com.docker.compose.project=$project" \
    --filter "label=com.docker.compose.volume=$1" | head -n 1
}
objects_volume=$(volume seaweedfs_data)
if [ -z "$objects_volume" ]; then
  printf 'No object-store volume found for Compose project %s (is the stack created?)\n' "$project" >&2
  exit 1
fi
objects_image=$("${compose[@]}" config --format json | python3 -c 'import json,sys; print(json.load(sys.stdin)["services"]["seaweedfs"]["image"])')

restart() {
  "${compose[@]}" start seaweedfs cloud >/dev/null 2>&1 || printf 'Warning: could not restart the stack; run: docker compose --project-name %s start\n' "$project" >&2
}
trap restart EXIT

printf 'Stopping the cloud service and the object store for a consistent copy...\n'
"${compose[@]}" stop cloud seaweedfs >/dev/null

printf 'Database dump...\n'
"${compose[@]}" exec -T postgres pg_dump --username figcraft --dbname figcraft --format custom --no-password > "$out/database.dump"
schema=$("${compose[@]}" exec -T postgres psql --username figcraft --dbname figcraft --tuples-only --no-align --command 'SELECT max(version) FROM _sqlx_migrations' | tr -d '[:space:]')

printf 'Object store copy...\n'
docker run --rm --entrypoint tar --volume "$objects_volume:/data:ro" --volume "$out:/out" "$objects_image" -C /data -czf /out/objects.tgz .

printf 'Secrets...\n'
if [ -d "$script_dir/secrets" ]; then
  tar -C "$script_dir" -czf "$out/secrets.tgz" secrets
fi

(
  cd "$out"
  {
    printf 'created_utc=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf 'compose_project=%s\n' "$project"
    printf 'schema_version=%s\n' "$schema"
    printf 'format=1\n'
  } > MANIFEST
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum database.dump objects.tgz MANIFEST > SHA256SUMS
  else
    shasum -a 256 database.dump objects.tgz MANIFEST > SHA256SUMS
  fi
  [ ! -f secrets.tgz ] || { if command -v sha256sum >/dev/null 2>&1; then sha256sum secrets.tgz; else shasum -a 256 secrets.tgz; fi; } >> SHA256SUMS
)

printf 'Backup written to %s (schema %s). Store it away from this machine.\n' "$out" "$schema"

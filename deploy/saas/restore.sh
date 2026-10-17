#!/usr/bin/env bash
# Restore a backup made by backup.sh into a stack that has been started at least once (so the
# database roles and the bucket exist) and that uses the same secrets. THIS REPLACES the stack's
# current database and objects.
#
#   deploy/saas/restore.sh --replace <backup-directory>
set -euo pipefail

if [ "${1:-}" != "--replace" ] || [ -z "${2:-}" ]; then
  printf 'usage: %s --replace <backup-directory>\n(this replaces the stack'"'"'s current data)\n' "$0" >&2
  exit 2
fi
backup=$2
script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
env_file=${FIGCRAFT_ENV_FILE:-}
if [ -z "$env_file" ]; then
  if [ -f "$script_dir/.env" ]; then env_file="$script_dir/.env"; else env_file="$script_dir/env.example"; fi
fi
project=${FIGCRAFT_COMPOSE_PROJECT:-figcraft-saas}
compose=(docker compose --project-name "$project" --env-file "$env_file" --file "$script_dir/compose.yaml")
# Optional extra Compose file (for example one that points the secrets somewhere else).
if [ -n "${FIGCRAFT_COMPOSE_OVERRIDE:-}" ]; then compose+=(--file "$FIGCRAFT_COMPOSE_OVERRIDE"); fi

for file in database.dump objects.tgz MANIFEST SHA256SUMS; do
  [ -f "$backup/$file" ] || { printf 'Not a complete backup: %s is missing\n' "$file" >&2; exit 1; }
done
printf 'Verifying checksums...\n'
(cd "$backup" && if command -v sha256sum >/dev/null 2>&1; then sha256sum --check --quiet SHA256SUMS; else shasum -a 256 -c --quiet SHA256SUMS; fi)

backup_schema=$(sed -n 's/^schema_version=//p' "$backup/MANIFEST")
volume() {
  docker volume ls --quiet \
    --filter "label=com.docker.compose.project=$project" \
    --filter "label=com.docker.compose.volume=$1" | head -n 1
}
objects_volume=$(volume seaweedfs_data)
[ -n "$objects_volume" ] || { printf 'No object-store volume found for project %s; start the stack once first\n' "$project" >&2; exit 1; }
objects_image=$("${compose[@]}" config --format json | python3 -c 'import json,sys; print(json.load(sys.stdin)["services"]["seaweedfs"]["image"])')

printf 'Stopping the cloud service and the object store...\n'
"${compose[@]}" stop cloud seaweedfs >/dev/null

printf 'Restoring the database (backup schema %s)...\n' "$backup_schema"
"${compose[@]}" exec -T postgres pg_restore --username figcraft --dbname figcraft --clean --if-exists --single-transaction --no-password < "$backup/database.dump"

printf 'Restoring the objects...\n'
docker run --rm --entrypoint sh --volume "$objects_volume:/data" --volume "$(cd "$backup" && pwd):/in:ro" "$objects_image" \
  -c 'find /data -mindepth 1 -delete && tar -C /data -xzf /in/objects.tgz'

printf 'Starting again...\n'
"${compose[@]}" start seaweedfs cloud >/dev/null
printf 'Restored. Check the stack with: docker compose --project-name %s ps\n' "$project"
printf 'If this is a new server, put the backed-up secrets in place BEFORE starting (secrets.tgz).\n'

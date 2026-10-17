#!/usr/bin/env bash
# Create the secret files a single-node FigCraft Cloud stack needs, with fresh random values.
#
#   deploy/saas/init-secrets.sh            # writes deploy/saas/secrets/ (refuses to overwrite)
#
# Run it once on the machine that hosts the stack, then keep the directory private and back it up
# with backup.sh. Never commit it: it is ignored by Git.
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
dir=${1:-"$script_dir/secrets"}
if [ -e "$dir/session_signing_key" ]; then
  printf 'Secrets already exist in %s; refusing to overwrite them.\n' "$dir" >&2
  exit 1
fi
command -v openssl >/dev/null || { printf 'openssl is required\n' >&2; exit 1; }
mkdir -p "$dir"
chmod 700 "$dir"
umask 077

postgres_password=$(openssl rand -hex 24)
migrator_password=$(openssl rand -hex 24)
runtime_password=$(openssl rand -hex 24)
redis_password=$(openssl rand -hex 24)
s3_access_key="FC$(openssl rand -hex 12)"
s3_secret_key=$(openssl rand -hex 24)
printf 'postgres://figcraft:%s@postgres:5432/figcraft\n' "$postgres_password" > "$dir/admin_database_url"
printf 'postgres://figcraft_migrator:%s@postgres:5432/figcraft\n' "$migrator_password" > "$dir/migration_database_url"
printf 'postgres://figcraft_runtime:%s@postgres:5432/figcraft\n' "$runtime_password" > "$dir/database_url"
printf '%s\n' "$migrator_password" > "$dir/migrator_password"
printf '%s\n' "$runtime_password" > "$dir/runtime_password"
printf 'redis://:%s@redis:6379/\n' "$redis_password" > "$dir/redis_url"
printf '%s\n' "$postgres_password" > "$dir/postgres_password"
printf '%s' "$redis_password" > "$dir/redis_password"
printf 'requirepass %s\n' "$redis_password" > "$dir/redis.conf"
printf '%s\n' "$s3_access_key" > "$dir/s3_access_key"
printf '%s\n' "$s3_secret_key" > "$dir/s3_secret_key"
# Reserved for an identity provider; the stack signs people in with passwords by default.
printf '%s\n' "$(openssl rand -hex 24)" > "$dir/oidc_client_secret"
printf '%s\n' "$(openssl rand -hex 32)" > "$dir/session_signing_key"
printf '{"identities":[{"name":"figcraft","credentials":[{"accessKey":"%s","secretKey":"%s"}],"actions":["Admin","Read","Write","List"]}]}\n' \
  "$s3_access_key" "$s3_secret_key" > "$dir/seaweedfs-s3-config.json"

# Docker Compose bind-mounts each secret file with its host permission bits, and the cloud image
# runs as a non-root user, so the files must be readable by others; the directory stays private.
chmod 644 "$dir"/*
printf 'Secrets written to %s (directory mode 700, files readable by the containers).\n' "$dir"

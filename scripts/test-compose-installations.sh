#!/bin/sh
# Compare the current unfunded demo with a second, disposable installation.
set -eu
cd "$(dirname "$0")/.."
query="SELECT encode(address,'hex') FROM wallets ORDER BY 1"
addresses() { docker compose exec -T db psql -XAt -v ON_ERROR_STOP=1 -U custody -d custody -c "$query"; }
first=$(addresses)
[ "$(printf '%s\n' "$first" | wc -l | tr -d ' ')" = 8 ] || {
    printf 'Run sh scripts/smoke-compose.sh on the reference installation first.\n' >&2; exit 1;
}
COMPOSE_PROJECT_NAME="custody-qa-$(openssl rand -hex 8)"
CUSTODY_DB_PASSWORD=$(openssl rand -hex 24)
CUSTODY_WEB_PORT=0
export COMPOSE_PROJECT_NAME CUSTODY_DB_PASSWORD CUSTODY_WEB_PORT
[ -z "$(docker volume ls -q --filter "label=com.docker.compose.project=$COMPOSE_PROJECT_NAME")" ] || exit 1
trap 'docker compose down --volumes >/dev/null' EXIT
docker compose run --rm --no-deps unlock-init
docker compose up -d --no-build
sh scripts/smoke-compose.sh
second=$(addresses)
unique=$(printf '%s\n%s\n' "$first" "$second" | sort -u | wc -l | tr -d ' ')
[ "$unique" = 16 ] || { printf 'Address reuse across installations\n' >&2; exit 1; }
printf 'Verified 16 distinct addresses across two independent installations.\n'

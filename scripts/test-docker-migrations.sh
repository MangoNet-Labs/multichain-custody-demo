#!/bin/sh
# Exercise the embedded SQLx migrations in the shipping app, without Compose or keys.
set -eu
cd "$(dirname "$0")/.."
id=$(openssl rand -hex 8)
db="custody-ci-pg-$id"
app="custody-ci-app:$id"
cleanup() {
    docker rm -fv "$db" >/dev/null 2>&1 || :
    docker image rm "$app" >/dev/null 2>&1 || :
}
trap cleanup EXIT
trap 'exit 1' INT TERM HUP
docker build --target app -t "$app" .
docker run -d --name "$db" --network none --tmpfs /var/lib/postgresql/data \
    -e POSTGRES_HOST_AUTH_METHOD=trust postgres:17-bookworm >/dev/null
sql() { docker exec "$db" psql -XAt -v ON_ERROR_STOP=1 -U postgres -c "$1"; }
migrate() {
    docker run --rm --network "container:$db" -e DATABASE_URL=postgresql://postgres@127.0.0.1/postgres \
        "$app" custody-web --migrate
}
i=0
until sql 'SELECT 1' >/dev/null 2>&1 && docker exec "$db" pg_isready -h 127.0.0.1 -U postgres >/dev/null; do
    i=$((i+1))
    [ "$i" -lt 60 ] || { docker logs "$db"; exit 1; }
    sleep 1
done
migrate
before=$(sql 'SELECT id FROM custody_installation')
[ -n "$before" ]
migrate
set -- migrations/*.sql
[ "$(sql 'SELECT count(*) FROM _sqlx_migrations WHERE success')" -eq "$#" ]
[ "$(sql 'SELECT id FROM custody_installation')" = "$before" ]
[ "$(sql "SELECT string_agg(id,',' ORDER BY id) FROM demo_users")" = 'alice,bob' ]
[ "$(sql 'SELECT count(*) FROM wallets')" = 0 ]
printf 'Fresh database and repeat migration passed; no wallets created.\n'

#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
# Do not inherit a developer's database or expose a TCP listener.
tmp=$(mktemp -d /tmp/custody-pg.XXXXXXXX)
cleanup() {
    if [ -f "$tmp/data/postmaster.pid" ]; then
        pg_ctl -D "$tmp/data" -m fast -w stop || return 1
    fi
    rm -rf "$tmp"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
initdb -D "$tmp/data" -U demo --no-locale --encoding=UTF8 --auth-local=trust --auth-host=reject >/dev/null
pg_ctl -D "$tmp/data" -l "$tmp/postgres.log" -o "-c listen_addresses='' -k $tmp" -w start
export DATABASE_URL="postgresql://demo@localhost/postgres?host=$tmp"
cargo test --locked -p custody-store --test postgres -- --ignored
cargo test --locked -p evm-reader --bin evm-reader -- --ignored
cargo test --locked -p custody-signer --tests -- --ignored
cargo test --locked -p custody-web --test browser_flow -- --ignored

case "${1:-}" in
--live-bsc)
    network=bsc-testnet
    unset BSC_START_HEIGHT
    sync_worker() { cargo run --quiet --locked -p evm-reader -- --sync; }
    ;;
--live-mango|--mango)
    network=mango-testnet
    unset MANGO_START_HEIGHT
    (cd mango-worker && cargo test --locked --test checkpoint)
    (cd mango-worker && cargo test --locked --bin mango-worker -- --ignored)
    (cd mango-worker && cargo test --locked --test postgres -- --ignored)
    cargo build --locked -p custody-signer
    export CUSTODY_TEST_SIGNER_BIN="$PWD/target/debug/custody-signer"
    (cd mango-worker && cargo test --locked --test payout -- --ignored)
    [ "$1" = "--live-mango" ] || exit 0
    sync_worker() { (cd mango-worker && cargo run --quiet --locked -- --sync); }
    ;;
'') exit 0 ;;
*) printf 'Unknown test mode: %s\n' "$1" >&2; exit 2 ;;
esac
    sync_worker
    first=$(psql "$DATABASE_URL" -XAtc "SELECT height FROM scan_cursors WHERE network='$network'")
    sleep 3
    sync_worker
    last=$(psql "$DATABASE_URL" -XAtc "SELECT height FROM scan_cursors WHERE network='$network'")
    count=$(psql "$DATABASE_URL" -XAtc "SELECT count(*) FROM scanned_blocks WHERE network='$network' AND height > $first")
    [ "$last" -gt "$first" ] && [ "$count" -eq "$((last - first))" ] || {
        printf 'Restart did not commit a new continuous range: first=%s last=%s rows=%s\n' "$first" "$last" "$count" >&2
        exit 1
    }
    printf 'Verified %s restart: %s -> %s (%s consecutive heights)\n' "$network" "$first" "$last" "$count"

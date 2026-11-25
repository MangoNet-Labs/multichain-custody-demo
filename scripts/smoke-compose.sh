#!/bin/sh
# Run against your local demo. Allocates public addresses; never funds wallets.
set -eu
cd "$(dirname "$0")/.."
case "${1:-}" in ''|--restart) ;; *) printf 'Usage: sh scripts/smoke-compose.sh [--restart]\n' >&2; exit 2;; esac
sql() { docker compose exec -T db psql -XAt -v ON_ERROR_STOP=1 -U custody -d custody -c "$1"; }
wait_sql() {
    deadline=$(( $(date +%s) + 120 ))
    until [ "$(sql "$1")" = t ]; do
        [ "$(date +%s)" -lt "$deadline" ] || { printf 'Timed out: %s\n' "$2" >&2; return 1; }
        sleep 2
    done
}
endpoint=$(docker compose port web 4310)
case "$endpoint" in 127.0.0.1:*) origin="http://$endpoint";; *) printf 'Expected loopback web port\n' >&2; exit 1;; esac
wait_sql 'SELECT count(*)=2 FROM scan_head_samples' 'both finalized head samples'
for user in alice bob; do
    for network in mango-testnet bsc-testnet; do
        status=$(curl -fsS --max-time 10 -o /dev/null -w '%{http_code}' -H "Origin: $origin" \
            --data "network=$network" "$origin/wallet/$user/deposit")
        [ "$status" = 303 ]
    done
done
wait_sql "SELECT count(*)=8 FROM wallets WHERE role IN ('hot','gas') OR owner_id IN ('alice','bob')" 'platform and user wallets'
if [ "${1:-}" = '--restart' ]; then
    [ "$(sql "SELECT NOT EXISTS(SELECT 1 FROM withdrawals WHERE status IN ('queued','signed','submitted','review')) AND NOT EXISTS(SELECT 1 FROM sweep_orders WHERE status IN ('signed','submitted','review')) AND NOT EXISTS(SELECT 1 FROM sweep_checks WHERE next_check IS NOT NULL)")" = t ] || {
        printf 'Finish pending payments and sweep checks before running the restart check.\n' >&2; exit 1;
    }
    wallet_sql="SELECT a.network,a.slot,encode(w.address,'hex') FROM wallet_allocations a JOIN wallets w ON w.key_ref=a.id::text ORDER BY a.network,a.slot"
    before=$(sql "$wallet_sql")
    bs=$(sql "SELECT height FROM scan_cursors WHERE network='bsc-testnet'")
    mg=$(sql "SELECT height FROM scan_cursors WHERE network='mango-testnet'")
    docker compose up -d --no-build --no-deps --force-recreate web signer bsc mango broadcast mango-broadcast
    docker compose exec -T signer custody-signer platform >/dev/null
    [ "$(sql "$wallet_sql")" = "$before" ] || { printf 'Wallet registration changed\n' >&2; exit 1; }
    wait_sql "SELECT bool_and(c.height>p.h AND c.height-p.h=(SELECT count(*) FROM scanned_blocks b WHERE b.network=c.network AND b.height>p.h AND b.height<=c.height)) FROM (VALUES ('bsc-testnet',$bs::bigint),('mango-testnet',$mg::bigint)) p(n,h) JOIN scan_cursors c ON c.network=p.n" 'contiguous post-restart blocks on both networks'
fi
sql "SELECT network,height FROM scan_cursors ORDER BY network"
printf 'Eight registered test wallets. No transactions signed or sent by this check.\n'

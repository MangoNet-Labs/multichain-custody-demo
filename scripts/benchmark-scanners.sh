#!/bin/sh
# Read real finalized testnet data into a disposable Compose project, never the demo.
set -eu
cd "$(dirname "$0")/.."
bsc_count=${BSC_BENCH_HEIGHTS:-1200}
mango_count=${MANGO_BENCH_HEIGHTS:-600}
for n in "$bsc_count" "$mango_count"; do
    case "$n" in ''|*[!0-9]*|0*) echo "Expected a height count from 1 to 5000" >&2; exit 2;; esac
    [ "$n" -le 5000 ] || { echo "Limit each benchmark to 5000 heights" >&2; exit 2; }
done
command -v jq >/dev/null
command -v curl >/dev/null
export COMPOSE_PROJECT_NAME="custody-bench-$(openssl rand -hex 12)"
export CUSTODY_DB_PASSWORD="$(openssl rand -hex 24)"
dc() { docker compose -p "$COMPOSE_PROJECT_NAME" "$@"; }
query() { dc exec -T db psql -XAt -v ON_ERROR_STOP=1 -U custody -d custody -c "$1"; }
tmp=$(mktemp -d)
cid=
cleanup() {
    if [ -n "$cid" ]; then docker rm -f "$cid" >/dev/null || :; fi
    dc down --volumes --remove-orphans >/dev/null
    rm -rf "$tmp"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

bsc_url=$(dc config --format json | jq -er '.services.bsc.environment.BSC_TESTNET_RPC_URL')
mango_url=$(dc config --format json | jq -er '.services.mango.environment.MANGO_REST_URL')
mango_url=${mango_url%/}
rpc() {
    curl -fsS --max-time 25 -H 'Content-Type: application/json' \
        -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":$2}" "$bsc_url"
}
head_height() {
    if [ "$1" = bsc ]; then
        hex=$(rpc eth_getBlockByNumber '["finalized",false]' | jq -er '.result.number | select(test("^0x[0-9a-fA-F]+$"))') || return 1
        printf '%d\n' "$hex"
    else
        curl -fsS --max-time 25 -H 'Accept: application/json' "$mango_url/checkpoints" |
            jq -er '.data.sequence_number | tonumber | select(. >= 0)'
    fi
}
sample() {
    if [ "$1" = bsc ]; then
        tag=$(printf '0x%x' "$2")
        rpc eth_getBlockReceipts "[\"$tag\"]" >"$tmp/receipts.json"
        txs=$(jq -er '.result | arrays | length' "$tmp/receipts.json")
        bytes=$(wc -c <"$tmp/receipts.json")
        printf '  block=%s receipts=%s receipts_json_bytes=%s\n' "$2" "$txs" "$((bytes))"
    else
        bytes=$(curl -fsS --max-time 25 -o /dev/null -w '%{size_download}' \
            -H 'Accept: application/bcs' "$mango_url/checkpoints/$2/full")
        printf '  checkpoint=%s full_bcs_bytes=%s\n' "$2" "$bytes"
    fi
}
benchmark() {
    service=$1 n=$2 var=$3 bin=$4
    network="$service-testnet"
    t0=$(date +%s)
    head0=$(head_height "$service")
    first=$((head0 - n + 1))
    [ "$first" -ge 1 ] || { echo "Insufficient history" >&2; exit 1; }
    printf '\n%s: first=%s initial_finalized=%s\n' "$network" "$first" "$head0"
    cid=$(dc run -d -T --no-deps -e "$var=$first" "$service" "$bin" --follow)
    while :; do
        if [ "$(docker inspect -f '{{.State.Running}}' "$cid")" != true ] ||
                [ "$(( $(date +%s) - t0 ))" -ge 600 ]; then
            docker logs --tail 20 "$cid" >&2
            echo "Reader stopped or did not drain the initial backlog within ten minutes" >&2
            exit 1
        fi
        last=$(query "SELECT height FROM scan_cursors WHERE network='$network'")
        [ -n "$last" ] && [ "$last" -ge "$head0" ] && break
        sleep 2
    done
    peak=$(docker exec "$cid" cat /sys/fs/cgroup/memory.peak 2>/dev/null || echo unavailable)
    docker stop -t 3 "$cid" >/dev/null
    head1=$(head_height "$service")
    elapsed=$(( $(date +%s) - t0 ))
    docker logs "$cid" 2>&1 | awk '/rate limited|retrying|net_catchup|remaining=[1-9]/'
    docker rm "$cid" >/dev/null
    cid=
    printf 'scanner_container_peak_bytes=%s\n' "$peak"
    last=$(query "SELECT height FROM scan_cursors WHERE network='$network'")
    # Verify this is the entire contiguous interval, not just a moved cursor.
    ok=$(query "WITH chain AS (SELECT height,parent,digest,lag(digest) OVER(ORDER BY height) AS prev FROM scanned_blocks WHERE network='$network')
        SELECT count(*)=$last-$first+1 AND min(height)=$first AND max(height)=$last
        AND bool_and(prev IS NULL OR parent=prev)
        AND (SELECT digest FROM chain WHERE height=$last)=(SELECT digest FROM scan_cursors WHERE network='$network') FROM chain")
    [ "$ok" = t ] && [ "$last" -ge "$head0" ] && [ "$head1" -ge "$last" ] ||
        { echo "Invalid finalized range or database continuity" >&2; exit 1; }
    awk -v t="$elapsed" -v w="$((last - first + 1))" -v g="$((head1 - head0))" -v lag="$((head1 - last))" \
        'BEGIN { if (!t) exit 1; printf "verified=%d seconds=%d committed/s=%.2f head/s=%.2f net_catchup/s=%.2f final_lag=%d\n",w,t,w/t,g/t,(w-g)/t,lag }'
    for h in "$first" "$((first+(last-first)/2))" "$last"; do sample "$service" "$h"; done
}

dc up -d --wait db
dc run --rm -T --no-deps migrate
benchmark bsc "$bsc_count" BSC_START_HEIGHT evm-reader
benchmark mango "$mango_count" MANGO_START_HEIGHT mango-worker
[ "$(query 'SELECT (SELECT count(*) FROM wallets)+(SELECT count(*) FROM journal)+(SELECT count(*) FROM withdrawals)+(SELECT count(*) FROM sweep_orders)')" = 0 ]

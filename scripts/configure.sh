#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
umask 077
if [ -e .env ]; then
    printf '%s\n' 'Keeping existing .env. Do not rotate credentials when restarting the demo.'
    exit 0
fi
password=$(openssl rand -hex 24)
tmp=$(mktemp .env.XXXXXXXX)
trap 'rm -f "$tmp"' EXIT
sed "s/^CUSTODY_DB_PASSWORD=$/CUSTODY_DB_PASSWORD=$password/" .env.example > "$tmp"
ln "$tmp" .env
printf '%s\n' 'Created private .env with a random database password.'

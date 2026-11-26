# Multichain Custody Demo

A Rust and PostgreSQL reference for deposits, sweeps and withdrawals on two testnets: **Mango** (native MGO and allowlisted Move coins) and **BSC** (BNB and allowlisted standard ERC-20 tokens). Each network has its own ledger balances; this is not a bridge.

The signer signs a payment, the broadcaster sends it, and the scanner settles it only after it finds the finalized outcome. To see why a gas top-up does not credit the user, [follow one deposit through the code](docs/FOLLOW_THE_MONEY.md).

**Local testnet use only.** There is no authentication: anyone with access to the UI can operate both demo accounts and pause payments. Do not expose it publicly or use production keys or funds.

## Docker quickstart

Install Docker Compose v2 with BuildKit and OpenSSL. From the project root, on a **new installation**:

This release uses a single [initial schema](migrations/0001_init.sql) and requires an empty database on first use. It does not migrate earlier demo installations. Keep any funded installation and its keys intact.

Choose the [testnet asset lists](docs/SCANNERS.md) before the first startup.
Compose validates both lists before starting scanners, signing or broadcasting.
You can [add tokens later](docs/SCANNERS.md#adding-a-token-to-an-existing-installation) without resetting wallets or replaying history. Only transfers after activation can credit a user.

```sh
sh scripts/configure.sh
docker compose build web mango signer
docker compose run --rm --no-deps unlock-init
docker compose up -d --no-build
docker compose ps -a
```

Open [Custody Desk](http://127.0.0.1:4310). In **Operations**, wait for the network you want to use to show a finalized cursor and its hot and gas wallets. Then open **Wallet**, select Alice or Bob, and create a deposit address. Test funds are not supplied.

Cargo fetches the pinned public Mango source during the build; you do not need a separate checkout. Only the UI port is published, on loopback.

To stop and restart the same installation, use `docker compose stop` and `docker compose up -d`. **Do not rerun `unlock-init` or use `down -v` to restart:** the database, keystore and unlock credential belong together.

## Documentation

| I want to… | Start here |
| --- | --- |
| Run a deposit or withdrawal, check scanner health, back up wallets or reconcile funds | [Operating the demo](docs/OPERATIONS.md) |
| Trace a transfer from chain evidence to a user's balance | [Follow a deposit](docs/FOLLOW_THE_MONEY.md) |
| Implement another chain and verify it against the same custody rules | [Chain integration contract](docs/CHAIN_INTEGRATION.md) |
| Build a reader, configure an RPC or understand which transfers count | [Mango and BSC scanners](docs/SCANNERS.md) |
| Understand payment states, gas top-ups, sponsored sweeps or retries | [Withdrawals and sweeps](docs/PAYMENTS.md) |
| Run regression tests, restart checks or a catch-up benchmark | [Testing](docs/TESTING.md) |
| Look up tables, relationships and amount units | [PostgreSQL schema](migrations/0001_init.sql) |

## License

The original code and documentation are [MIT licensed](LICENSE). Retain the copyright and license notices when redistributing. Third-party dependencies and Mango protocol sources retain their own licenses; this license does not grant rights to third-party names or logos.

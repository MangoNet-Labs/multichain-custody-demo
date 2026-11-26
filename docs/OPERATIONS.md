# Operating the demo

Start with the [Docker quickstart](../README.md#docker-quickstart). Commands on this page assume the project root. The UI, PostgreSQL, two scanners, signer and broadcasters run locally; the scanners and payment workers use public testnet nodes.

## First deposit and withdrawal

1. In **Operations**, find the network you want to use. Once that scanner has saved its starting height and digest (its *cursor*), the signer creates the network's hot and gas wallets. One network can be ready while the other is unavailable.
2. In **Wallet**, select Alice or Bob and **Create address** for that network. Refresh after the signer registers the wallet. Send only supported testnet funds to the displayed deposit address. Look under **Activity → Deposits** for the finalized transaction. On BSC, use a direct transfer for native BNB; a contract's internal BNB transfer is not credited.
3. Fund the network's **hot wallet** with the payout asset and native gas. A token withdrawal needs BNB on BSC or MGO on Mango; the user's token principal is separate from the fee. A credited deposit need not be swept first if the hot wallet already has funds. Fund the separate **gas wallet** to enable gas top-ups on BSC or sponsored sweeps on Mango.
4. Under **Withdraw**, enter an external address on the same network and an amount. Expand the order in **Activity → Withdrawals** to see its status, digest, finalized height and platform fee. A submitted transaction is not a completed payment; see [payment states](PAYMENTS.md#withdrawal-states).

## Configuration and scanner health

`scripts/configure.sh` creates a private `.env` with a random database password. Configure the node URLs and UI port there before starting; see [.env.example](../.env.example). For provider requirements, inclusive start heights and tuning, see [Scanners](SCANNERS.md).

| Operations status | Meaning |
| --- | --- |
| No cursor | This network has not initialized. Do not publish deposit addresses or skip the missing height. |
| Waiting for finality | The reader could not obtain or validate a finalized head. An old head sample alone does not mean finality failed. |
| Catching up | Blocks have recently committed, but the cursor is behind the sampled head. |
| Scan interrupted at height N | N is the first uncommitted height of a failed range. The reader resumes from its committed cursor. |

Use `docker compose logs --tail=30 mango bsc signer` to distinguish node errors from wallet provisioning issues. **Operations → Pause payments** stops new signing and sends, not scanning or settlement. It cannot recall an in-flight transaction.

A Mango withdrawal in `review` blocks all further withdrawals from the Mango hot wallet. The global **Resume payments** button does not resolve an order in review. Inspect the signed digest and finalized checkpoint before deciding how to resolve it; do not retry it as a new payment.

## Preserve wallets across restarts

Alice and Bob exist on every install, but their addresses do not. Each account gets one random deposit key per network. The database holds a key reference, not the private key. On BSC, BNB and tokens share the same deposit address but have separate balances.

- Stop the stack with `docker compose stop`; restart with `docker compose up -d` (or add `--build` for source changes). Do not remove named volumes.
- To move the installation, stop the old instance and back up the matching PostgreSQL database, encrypted keystore and separately held unlock credential. Restore all three together. Do not generate a replacement key for an address that has already been issued.
- Compose runs the [schema migration](../migrations/0001_init.sql) automatically. The public baseline is for a fresh installation, not an upgrade from a pre-release database. Do not clear SQLx history to force it to run.
- Earlier database layouts are unsupported. Use a separate Compose project and new volumes for a new installation; do not reuse the old keystore with a new database. Keep the old database, keystore and unlock credential together while it holds funds or pending payments. The one-shot `assets` service validates configuration before the workers start.

## Native wallet tools

Outside Docker, set `DATABASE_URL` to a dedicated demo PostgreSQL database. With SQLx CLI, `sqlx migrate run` applies the migrations. The schema seeds the demo users but no keys, addresses or funds. Start the local UI with `cargo run --locked -p custody-web` (default `127.0.0.1:4310`).

Initialize each [scanner](SCANNERS.md) you intend to use against the same database. To provision wallets without the UI:

```sh
# Only for a fresh native installation
cargo run --locked -p custody-signer -- init-unlock
cargo run --locked -p custody-signer -- platform
cargo run --locked -p custody-signer -- deposit alice mango-testnet
cargo run --locked -p custody-signer -- deposit bob bsc-testnet
```

`platform` and `deposit` reuse registered addresses. The defaults are `CUSTODY_KEYSTORE=keystore` and `CUSTODY_UNLOCK_FILE=secrets/unlock.key`; the signer requires private directories (0700) and files (0600). For an independent test recipient, run:

```sh
mkdir -p -m 700 secrets
cargo run --locked -p custody-signer -- external-test-wallets secrets/external-acceptance
```

This creates an encrypted external wallet set and prints only public addresses. It is not mounted in Compose. Running the command again verifies the existing keys.

## Reconciliation

From a running installation, compare the ledger with the chain at the **same finalized height**:

```sh
docker compose exec -T bsc evm-reader --reconcile
docker compose exec -T mango mango-worker --reconcile
```

BSC needs historical BNB balances and ERC-20 `balanceOf` at the chosen block. Mango rebuilds a separate in-memory coin inventory from full checkpoints, not from the scanner's coin table. The first audit starts at the installation's anchor with zero holdings for newly generated addresses. A matched prior audit can be reused for the same set of wallets. The node must serve that range; no raw checkpoints are written to disk.

In **Operations → Reconciliation**, compare book and chain totals, available and frozen balances, capital, fees and per-wallet differences. A proven mismatch pauses new payments **across both networks**. Fix the cause, rerun the affected network's audit, then explicitly resume payments. An RPC error neither proves a mismatch nor clears a hold.

**Pre-listing balances (not credited)** are custody holdings observed between address issuance and listing, not user liabilities. A sweep can move these holdings without creating a deposit. Only the finalized scanner settles payments; an RPC coin page does not update the ledger.

For missing historical platform funding entries, `docker compose exec -T bsc evm-reader --backfill-funding HEIGHT` or `docker compose exec -T mango mango-worker --backfill-funding HEIGHT` requires a complete block matching the previously committed evidence. Neither command sends funds, advances the cursor or credits a user.

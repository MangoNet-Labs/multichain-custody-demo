# Mango and BSC scanners

Both readers fetch several heights concurrently but commit them **in order**, one database transaction per height. A gap stops delivery. The deposit, its journal entry and the cursor either commit together or roll back together. Replaying identical evidence does not credit a user twice; conflicting evidence or the wrong parent digest stops the scan.

| | Mango Testnet | BSC Testnet (chain ID 97) |
| --- | --- | --- |
| Read from the node | One complete BCS checkpoint per sequence, decoded locally | Full block transactions and all receipts for that block |
| Assets credited | Native MGO and allowlisted address-owned `0x2::coin::Coin<T>` | Direct native BNB transfers and standard ERC-20 transfers from allowlisted contracts |
| Required endpoint | Mango REST with full checkpoint data (`MANGO_REST_URL`) | JSON-RPC supporting `finalized` and `eth_getBlockReceipts` (`BSC_TESTNET_RPC_URL`) |

On a fresh database each network saves its own finalized head as the anchor, then processes the **following** height. For historical replay, `MANGO_START_HEIGHT` or `BSC_START_HEIGHT` specifies the first height to include on an empty installation. Existing cursors always resume; changing a start variable does not skip a gap or reset the database. These are trusted-node readers, not independent validator quorum light clients.

## BSC: inspect a block or start indexing

Run from the project root. Inspection verifies the chain ID and genesis, fetches matching transactions and receipts and makes no database writes. Without an address filter it checks the block but does not match deposits.

```sh
cargo run --locked -p evm-reader
BSC_RECENT_BLOCKS=8 cargo run --locked -p evm-reader
cargo run --locked -p evm-reader -- 0x0000000000000000000000000000000000000001
```

To index deposits, set `DATABASE_URL` to a dedicated demo database. Choose `--sync` to catch up to one sampled finalized head and exit, or `--follow` to keep polling:

```sh
cargo run --locked -p evm-reader -- --sync
cargo run --locked -p evm-reader -- --follow
```

For native BNB, the reader matches a successful transaction's top-level `to` and positive `value`. It does **not** trace internal contract transfers. For tokens it filters logs from successful receipts by emitting contract, standard `Transfer` event and recipient. Neither path needs an RPC call per transaction. An internal movement between managed wallets, such as a gas top-up, is not an external deposit.

The [BSC allowlist](../config/bsc-testnet-tokens.json) includes test MGO at `0xEdB22908ca8B42F0BbE0eC337F5F21948c83f046` (9 decimals). This is separate from Mango native MGO. Use `BSC_TOKEN_CONFIG` for an alternate host file in Compose, or `BSC_TOKEN_ALLOWLIST` for a native worker. The initial list applies from the installation anchor; additions use the boundary described below.

On a fresh database the schema installs only the two native currencies. Each scanner registers its own allowlist (which may be empty) and marks it ready in the same transaction. A scan cursor alone does not enable sweeps. The signer service retries until that network is ready; a one-shot sweep reports the pending registration and can be rerun afterwards. No manual startup ordering is required.

## Mango: build and inspect the worker

From the project root:

```sh
cd mango-worker
cargo build --locked
cargo test --locked
cargo run --locked -- inspect 106658
```

The worker uses Rust 1.88.0. Cargo fetches the pinned Git versions of `mgo-types` and `shared-crypto` declared in the [worker manifest](../mango-worker/Cargo.toml); no separate Mango checkout is required. The historical example corresponds to a [checkpoint fixture](../mango-worker/tests/fixtures/README.md) that can also be replayed offline.

From the same `mango-worker` directory, set `DATABASE_URL` and run `cargo run --locked -- --sync` or `cargo run --locked -- --follow`. The reader checks genesis, parent, transaction, effects and contents digests. It uses finalized coin and ownership changes, not a per-transaction balance RPC.

To inspect the REST format without building the worker:

```sh
REST=https://fullnode.testnet2.mangonetwork.io/rest
curl -fsS -H 'Accept: application/json' "$REST/checkpoints"
# Set SEQ to a sequence number from the response above.
curl -fsS -H 'Accept: application/bcs' -o checkpoint.bcs "$REST/checkpoints/${SEQ:?set SEQ first}/full"
```

The response is raw BCS `CheckpointData`, not a framed ingestion `.chk` file. The running scanner does not require a local checkpoint archive.

### Enable a Mango deposit token

Edit [the Mango allowlist](../config/mango-testnet-coins.json). It defaults to `[]`; native MGO is always enabled. Supply the verified testnet type **inside** `Coin<T>` and its display precision, not a coin object ID or a ticker. For example (illustrative type, not a deployed token):

```json
[{"coin_type":"0xabc::usd::USD","symbol":"USD","decimals":6}]
```

Compose mounts this file read-only; use `MANGO_COIN_CONFIG` to select the host file. Outside Docker, `MANGO_COIN_ALLOWLIST` selects the file. Once registered, the coin is available for deposits and withdrawals. To enable automatic collection, add its canonical type and threshold to the [sweep rules](../config/sweep.example.toml) and restart the signer.

Each successful non-system transaction produces at most one credit per recipient and canonical coin type: the positive net change of its address-owned coin objects. Splitting or merging coins without a net gain is not income. Gas and storage rebates affect native MGO only. Tickers can coincide; raw balances and deposit keys never use the ticker. Wrapped, object-owned and arbitrary Move assets are not supported.

### Adding a token to an existing installation

1. Append the verified contract or Move type, symbol and decimals to that network's list. Existing entries cannot be removed or redefined. Reordering them is harmless.
2. Stop the old workers and recreate the stack with the updated files (keep all volumes). The startup check validates the list. The scanner registers each addition under its cursor lock: if the last committed height is **H**, transfers through **H** are pre-listing and credits start at **H+1**. The boundary uses the database cursor, not the node's latest head or the time the file was edited.
3. To collect the new asset automatically, add its threshold to the sweep configuration and restart the signer after scanner registration. Listing itself does not schedule any wallet for collection.

Both scanners already read the complete finalized data. Before listing, the BSC reader extracts signed changes from standard ERC-20 Transfer logs (not NFTs); Mango calculates net changes in address-owned `Coin<T>` balances. The store accumulates these changes only for registered wallets with coverage since issuance, in one sparse `opening_balances` table. No deposit, journal credit or Mango object inventory is created for the unlisted asset. A new listing freezes the amount: it is a custody holding, **not a user balance**. There is no per-wallet activation RPC or replay.

Example: a deposit address receives 4 tokens before listing and 2 after. Its user gets 2; a normal threshold-triggered sweep may move all 6. An address with only pre-listing holdings gets no special sweep. On Mango, a due sweep uses one current coin page to build a transaction, not to recalculate the opening or credit a user.

Only list standard transfer-accounted assets: rebasing tokens and tokens with silent balance changes cannot be reconciled from transfer logs.

## Catch-up and limits

| Setting | Default | Limits |
| --- | ---: | --- |
| `BSC_FETCH_CONCURRENCY` / `BSC_READY_CAPACITY` | 4 / 8 | In-flight heights / decoded heights waiting to commit. Each height may request its block and receipts concurrently. |
| `MANGO_FETCH_CONCURRENCY` / `MANGO_READY_CAPACITY` | 2 / 8 | In-flight checkpoints / decoded results waiting to commit. |
| `MANGO_MAX_CHECKPOINT_MIB` / `MANGO_INFLIGHT_MIB` | 32 / 64 | Maximum raw response size / total reserved raw-body capacity. Decoded objects and transport overhead are additional. Oversized checkpoints stop the scan. |

Follow mode backs off on HTTP 429, halves concurrency (minimum one), and restores at most one slot per minute after successful ranges. It does not skip heights or replace finality with a confirmation count. One-shot commands report errors rather than retry indefinitely.

Logs report fetch/decode and commit times, queue depth and committed heights/s. `remaining` is relative to the sampled target. For a moving head, compare `head/s` with `committed/s`: a positive `net_catchup/s` means lag is shrinking. See [Testing](TESTING.md#live-checks) for the benchmark or [Operations](OPERATIONS.md#configuration-and-scanner-health) for status messages.

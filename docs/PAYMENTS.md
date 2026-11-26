# Withdrawals and sweeps

Each network has a hot wallet that receives sweeps and pays withdrawals, and a separate gas wallet. The amount credited to a user is a ledger balance, not the amount in their deposit address. A withdrawal can proceed before a sweep if the hot wallet has the asset and gas. User withdrawal fees are zero; network fees are a platform expense.

## Withdrawal states

| State | What has been proven |
| --- | --- |
| `queued` | The request was accepted and its principal moved from available to frozen. No payment has been signed. |
| `consolidating` (Mango) | The hot wallet is combining its own coins for this order. The principal remains frozen; nothing has been paid to the recipient. |
| `signed` / `submitted` | Signed bytes are stored for broadcast or have been submitted. Funds remain frozen; an RPC acknowledgement is not settlement. |
| `completed` | The finalized scanner matched the payment to the order and verified the expected transfer. |
| `failed` | Confirmed execution failure releases the frozen principal. Actual network gas is accounted for separately. |
| `review` | Conflicting evidence or a missing expected transfer requires investigation. The order is not automatically refunded. |

The [request handler for withdrawals](../crates/custody-store/src/withdrawals.rs) binds each request ID to the user, network, recipient, asset and amount. An identical retry cannot freeze funds twice. The signer persists the signed transaction before broadcast; a lost acknowledgement is not permission to create a new payment. Pausing stops new signatures and sends, not the settlement of transactions already on chain.

## BSC payments

The signer checks the frozen order, testnet identity, available hot-wallet funds, nonce and estimated gas. `BSC_MAX_PAYOUT_FEE_WEI` caps the fee reserved per transaction (default 0.001 test BNB). The nonce and signed bytes are saved together. The broadcaster has no keys and retries the same bytes at least 30 seconds apart. A successful ERC-20 receipt without the expected token transfer is not proof of payment.

For native development, `custody-signer serve` checks both networks' asset lists at startup, even if you only run the BSC scanner. Docker already includes `mango-worker`. Outside Docker, build the [Mango worker](SCANNERS.md#mango-build-and-inspect-the-worker) first, then set its path in the terminal that will run the signer (or put it on `PATH`):

```sh
# From the project root, using Cargo's default target directory
(cd mango-worker && cargo build --locked --bin mango-worker)
export MANGO_WORKER_BIN="$PWD/mango-worker/target/debug/mango-worker"
```

Run these as **separate processes** from the project root against the same initialized demo database:

```sh
cargo run --locked -p custody-signer -- serve
cargo run --locked -p evm-reader -- --broadcast
cargo run --locked -p evm-reader -- --follow
```

For one iteration use `custody-signer bsc-prepare-once` and `evm-reader --broadcast-once`. Only the finalized reader can complete the order.

## Mango payments

For a native signer, follow the worker setup [above](#bsc-payments). Using `MANGO_RPC_URL`, the helper checks genesis and reads only the **first 50 coins per required type**: native MGO for a native payout, or one MGO page for gas and one token page for a token payout. If the page covers the amount, pay directly. A native withdrawal uses `new_pay_mgo`. A token withdrawal selects only the ordered `Coin<T>`, merges inputs if needed, and calls `0x2::pay::split_and_transfer<T>` for the exact raw amount. The dry-run must succeed and show the exact token gain at the recipient. The default `MANGO_PAYOUT_GAS_BUDGET` is 10000000 raw units (0.01 MGO).

The signer reserves each selected coin ID and version atomically with the signed payload. A separate `mango-worker --broadcast` process reconstructs the transaction and verifies its Ed25519 signature. The checkpoint scanner must prove the exact recipient gain. An unexpected consumer of a reserved coin or a missing payment leaves funds frozen for review. Net gas, including a storage rebate, is booked separately.

If the page is short and `hasNextPage` is false, wait for funds. If more coins exist and a merge is possible, check `mgox_getBalance` once for this asset. If the total covers the withdrawal, merge the first page; otherwise wait without spending gas. No withdrawal follows a coin cursor. The first MGO page must itself cover the gas budget to sign either a payment or a merge.

Native MGO is paid back to the same wallet; token objects are merged into one of its existing coins. The signed consolidation belongs to the original withdrawal. Only after checkpoint confirmation does the next signer run read a fresh first page. Other withdrawals from this hot wallet wait; deposit scanning and sponsored sweeps can continue. The platform pays network fees for each batch.

Withdrawal history labels the latest consolidation digest separately from the recipient's payment. A failed or conflicting consolidation sends the original order to review; a broadcast timeout merely retries the same signed bytes. Only the final payment can settle the principal.

## Sweep rules and gas

Thresholds are set per network and asset in [sweep.example.toml](../config/sweep.example.toml). A small deposit is still credited; the **confirmed balance** determines when to sweep. Removing a rule disables new sweeps for that asset. To apply edited rules restart the signer; when changing the host file via `CUSTODY_SWEEP_CONFIG`, recreate the Compose signer with the new mount.

| | BSC | Mango |
| --- | --- | --- |
| Trigger and gas | Native BNB is compared with the threshold **before** gas is deducted. For tokens, reaching the token threshold triggers the check; if BNB is insufficient, the gas wallet sends a separate top-up. | Compare the asset's confirmed total (opening plus posted movements), not a single page, with its threshold. Select at most 50 deposit coins and leave room for the sponsor's gas inputs. The deposit address does not need MGO. |
| Order of operations | When both token and BNB qualify, sweep the token first. Wait for finalized top-up evidence before spending it, then check remaining BNB after the finalized token transfer. | The deposit wallet and gas sponsor sign **one** transaction. The dry-run must show the full amount reaching hot; gas does not reduce the amount swept. Recheck the balance after each finalized batch. |

BSC checks only deposit addresses with new receipts or pending retries. The signer verifies the indexed block hash and reads balances at that height. Missing historical state defers signing; it is not a zero balance. Threshold changes wake waiting addresses. See **Operations → Recent sweeps / Sweep checks** or use `custody-signer bsc-sweep-once` for one native iteration.

A Mango sweep calls `mgox_getCoins` once with a null cursor for the deposit address and coin type. If the page is empty there is nothing to send. Otherwise one sponsored `TransferObjects` sends the entire page to the hot wallet, for native MGO and allowlisted Move tokens alike. There is no merge or minimum page amount. A page containing only zero-balance coins still transfers those objects and uses sponsor gas; it creates no user deposit. If the page exceeds confirmed holdings or references stale versions, retry the whole page rather than sending a subset.

Before both wallets sign, the dry-run must match the expected hot-wallet balance change. The signer reserves the exact input versions. While the order is outstanding, it does not request another page; a lost broadcast acknowledgement retries the stored transaction. The scanner checks that each deposit coin changed ownership to the hot wallet in the finalized transaction. Only then, if the remaining confirmed total still meets the threshold, does the signer request a fresh **first** page.

For the code and tables behind each step, [follow a deposit](FOLLOW_THE_MONEY.md). To verify actual custody funds against the ledger, use [reconciliation](OPERATIONS.md#reconciliation).

Both networks recheck a deposit address 30, 60 and 120 seconds after consecutive
finalized sweep failures. The fourth failure ends automatic retries for that
address; see the reason in Operations. A successful sweep resets the budget.
A successful BSC gas top-up does not. A broadcast timeout is not an execution
failure: keep the existing signed order. A conflicting final outcome stays in review.

# Withdrawals and sweeps

Each network has a hot wallet that receives sweeps and pays withdrawals, and a separate gas wallet. The amount credited to a user is a ledger balance, not the amount in their deposit address. A withdrawal can proceed before a sweep if the hot wallet has the asset and gas. User withdrawal fees are zero; network fees are a platform expense.

## Withdrawal states

| State | What has been proven |
| --- | --- |
| `queued` | The request was accepted and its principal moved from available to frozen. No payment has been signed. |
| `signed` / `submitted` | Signed bytes are stored for broadcast or have been submitted. Funds remain frozen; an RPC acknowledgement is not settlement. |
| `completed` | The finalized scanner matched the payment to the order and verified the expected transfer. |
| `failed` | Confirmed execution failure releases the frozen principal. Actual network gas is accounted for separately. |
| `review` | Conflicting evidence or a missing expected transfer requires investigation. The order is not automatically refunded. |

The [request handler for withdrawals](../crates/custody-store/src/withdrawals.rs) binds each request ID to the user, network, recipient, asset and amount. An identical retry cannot freeze funds twice. The signer persists the signed transaction before broadcast; a lost acknowledgement is not permission to create a new payment. Pausing stops new signatures and sends, not the settlement of transactions already on chain.

## BSC payments

The signer checks the frozen order, testnet identity, available hot-wallet funds, nonce and estimated gas. `BSC_MAX_PAYOUT_FEE_WEI` caps the fee reserved per transaction (default 0.001 test BNB). The nonce and signed bytes are saved together. The broadcaster has no keys and retries the same bytes at least 30 seconds apart. A successful ERC-20 receipt without the expected token transfer is not proof of payment.

For native development, run these as **separate processes** from the project root against the same initialized demo database:

```sh
cargo run --locked -p custody-signer -- serve
cargo run --locked -p evm-reader -- --broadcast
cargo run --locked -p evm-reader -- --follow
```

For one iteration use `custody-signer bsc-prepare-once` and `evm-reader --broadcast-once`. Only the finalized reader can complete the order.

## Mango payments

The signer image includes the Mango worker. For a native signer, set `MANGO_WORKER_BIN` to the [built worker](SCANNERS.md#mango-build-and-inspect-the-worker) or put it on PATH. Using `MANGO_RPC_URL`, the helper checks genesis and pages the hot wallet's coins. A native withdrawal uses `new_pay_mgo`. A token withdrawal selects only the ordered `Coin<T>`, merges inputs if needed, and calls `0x2::pay::split_and_transfer<T>` for the exact raw amount. Gas inputs are separate native MGO objects. Before signing, the dry-run must succeed and show the exact token gain at the recipient. The default `MANGO_PAYOUT_GAS_BUDGET` is 10000000 raw units (0.01 MGO). Insufficient token balance or native gas leaves the order unsigned.

The signer reserves each selected coin ID and version atomically with the signed payload. A separate `mango-worker --broadcast` process reconstructs the transaction and verifies its Ed25519 signature. The checkpoint scanner must prove the exact recipient gain. An unexpected consumer of a reserved coin or a missing payment leaves funds frozen for review. Net gas, including a storage rebate, is booked separately.

## Sweep rules and gas

Thresholds are set per network and asset in [sweep.example.toml](../config/sweep.example.toml). A small deposit is still credited; the **confirmed balance** determines when to sweep. Removing a rule disables new sweeps for that asset. To apply edited rules restart the signer; when changing the host file via `CUSTODY_SWEEP_CONFIG`, recreate the Compose signer with the new mount.

| | BSC | Mango |
| --- | --- | --- |
| Trigger and gas | Native BNB is compared with the threshold **before** gas is deducted. For tokens, reaching the token threshold triggers the check; if BNB is insufficient, the gas wallet sends a separate top-up. | Compare each configured asset's finalized balance separately. A sweep selects one coin type and up to 128 total inputs including the sponsor's native MGO. The deposit address does not need MGO. |
| Order of operations | When both token and BNB qualify, sweep the token first. Wait for finalized top-up evidence before spending it, then check remaining BNB after the finalized token transfer. | The deposit wallet and gas sponsor sign **one** transaction. The dry-run must show the full amount reaching hot; gas does not reduce the amount swept. Recheck the balance after each finalized batch. |

BSC checks only deposit addresses with new receipts or pending retries. The signer verifies the indexed block hash and reads balances at that height. Missing historical state defers signing; it is not a zero balance. Threshold changes wake waiting addresses. See **Operations → Recent sweeps / Sweep checks** or use `custody-signer bsc-sweep-once` for one native iteration.

For the code and tables behind each step, [follow a deposit](FOLLOW_THE_MONEY.md). To verify actual custody funds against the ledger, use [reconciliation](OPERATIONS.md#reconciliation).

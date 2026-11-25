# Follow a deposit through the code

Suppose Bob deposits 10 test MGO on BSC. The platform supplies BNB and sweeps
the tokens; Bob later withdraws 3 MGO. His final **ledger balance is 7 MGO**,
even though his **deposit address holds zero MGO** after the sweep. The ledger
records what the platform owes Bob, not where it keeps the tokens. The amounts
below illustrate the code path; a fresh installation starts with no funds.

| Step | Start reading here | What changes, and why |
| --- | --- | --- |
| 1. Issue an address | [`wallets::request`](../crates/custody-store/src/wallets.rs), [`Signer::resolve`](../crates/custody-signer/src/lib.rs) | Once **BSC's** cursor exists, Bob can request a deposit wallet. The allocation is unique per network/user slot. The signer registers the address while holding the BSC cursor lock; the web app never sees a key. Mango need not be online. |
| 2. Credit the deposit | [`scan_matching`](../crates/evm-reader/src/lib.rs), [`apply_all`](../crates/custody-store/src/lib.rs) | In a finalized block, a successful receipt contains the allowlisted MGO contract's transfer to Bob. With 9 decimals, 10 MGO is `10000000000` raw units. One DB transaction writes the deposit evidence, credits `user:bob:available` in `journal`, schedules a sweep check and advances the cursor. An identical block replay does not post another credit. |
| 3. Supply gas | [`prepare_bsc_sweep`](../crates/custody-signer/src/sweep.rs), [`settle_bsc`](../crates/custody-store/src/sweeps.rs) | The token balance meets the sweep threshold but the address needs BNB. The gas wallet sends the required top-up to Bob's deposit address. The finalized scanner records this managed-to-managed transfer in `internal_receipts` and settles the `gas_topup` order. Bob gets no BNB ledger credit. |
| 4. Sweep the tokens | [sweep signer](../crates/custody-signer/src/sweep.rs), [BSC broadcaster](../crates/evm-reader/src/broadcast.rs) | After the top-up is confirmed, the signer prepares a separate token transaction. The order and signed bytes are stored in `sweep_orders` and `evm_payouts`; the broadcaster needs no key. When the tokens reach the hot wallet, Bob still has 10 MGO available: the platform moved funds it already controlled. |
| 5. Freeze and send a withdrawal | [`withdrawals::submit`](../crates/custody-store/src/withdrawals.rs), [`prepare_bsc`](../crates/custody-signer/src/bsc.rs) | Requesting 3 MGO moves that amount from available to frozen; resubmitting the same request ID and terms does not freeze it twice. The signer checks the frozen order and reserves a nonce. A lost broadcast acknowledgement is retried with the **same signed bytes**, not a second payment. |
| 6. Settle, then reconcile | [`payouts::settle`](../crates/custody-store/src/payouts.rs), [BSC reconciliation](../crates/evm-reader/src/reconcile.rs) | Once the finalized scanner matches the payment and the token transfer to the recipient, Bob has **7 available, 0 frozen**. Actual BNB gas is a platform expense. A proven revert releases principal; conflicting evidence leaves it frozen for review. Reconciliation compares the books and chain balances at one confirmed height. |

**Inspect your own run.** The following read-only query shows Bob's BSC MGO
postings, including the transfer into and out of frozen. Filter by contract,
not ticker: native Mango MGO belongs to a different ledger.

```sql
SELECT id, account, delta AS raw_change
FROM journal_entries
WHERE network = 'bsc-testnet'
  AND asset = decode('edb22908ca8b42f0bbe0ec337f5f21948c83f046', 'hex')
  AND account IN ('user:bob:available', 'user:bob:frozen')
ORDER BY id, account;
```

## What changes on Mango?

- **Evidence:** [`decode_full`](../mango-worker/src/lib.rs) decodes the complete BCS checkpoint locally. [`apply_mango_full`](../crates/custody-store/src/lib.rs) commits the resulting receipts, wallet coin versions and cursor together. There is no ERC-20 log and no per-transaction RPC loop.
- **Gas:** [the sponsored sweep](../crates/custody-signer/src/mango_sweep.rs) uses two signatures on **one transaction**: the deposit wallet supplies MGO and the sponsor supplies gas coins. There is no preliminary gas deposit to credit. The signer records expected internal outputs before broadcast so a positive balance change is not mistaken for a new customer deposit.
- **Duplicate prevention:** Mango reserves coin object IDs **and versions**, not an EVM nonce. The [broadcaster](../mango-worker/src/payments.rs) verifies the signed transaction and the [checkpoint settlement code](../crates/custody-store/src/mango.rs) checks its actual outcome. The ledger distinction between available, frozen and platform funds is unchanged.

## Reproduce the important cases

Run `sh scripts/test-postgres.sh --mango` from the project root. This uses a temporary local database,
fixtures and local RPC doubles. Start with
[`sweeps_token_gas_topup_then_native_without_user_debit`](../crates/custody-signer/tests/payout.rs)
for internal BNB funding, the [store replay tests](../crates/custody-store/tests/postgres.rs)
for exactly-once credit, and [Mango payout tests](../mango-worker/tests/payout.rs)
for sponsor signatures and settlement.

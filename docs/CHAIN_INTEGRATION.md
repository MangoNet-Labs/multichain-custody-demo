# Chain integration contract

A chain adapter answers **what happened on its network**. The shared store decides
**what to post to the ledger**. An RPC acknowledgement, an SDK success response
and an explorer status are not substitutes for finalized evidence.

| Boundary | Shared responsibility | Required adapter output |
| --- | --- | --- |
| Startup | Allow additions only. Under the cursor lock, freeze the opening as of H and activate credits from H+1. Resume the stored cursor; the configured first height applies only to a new cursor. | Expected network identity, canonical asset IDs and decimal precision. Native currency is implicit; a symbol is never an identifier. |
| Ingestion | Bounded parallel fetch and ordered commit. Commit evidence, ledger postings, settlement and cursor in one DB transaction. Reject gaps, conflicting replays and mismatched parents. | A finalized height, digest, parent digest and complete evidence for that height. An incomplete response is an error, not an empty block. |
| Deposits | Credit a registered deposit wallet once. Classify internal sweeps and gas funding separately. Record external hot-wallet funding as platform capital, not a user credit. | Integer amounts and a stable transfer identity. Identify internal transfers using the signed intent where a sender alone is insufficient. |
| Pre-listing | Aggregate signed changes by network, asset and managed address; no journal or sweep trigger. Start observation when a new wallet is issued. | Exact net changes for standard assets in the finalized data, including debits. No asset-specific opening table or balance RPC. |
| Withdrawals | Freeze principal once per request. Persist signed bytes before sending. Lost response: keep the freeze and rebroadcast the same transaction. Never infer failure from elapsed time. | Build and sign the exact frozen asset, amount and recipient. Provide the signed digest, replay protection and finalized execution evidence including fees. |
| Sweeps | Compare confirmed holdings with the configured threshold. Serialize unresolved orders for a deposit address. Only a finalized execution failure consumes a retry. | Fresh spendable inputs, fee payer and the expected destination. Confirm the intended internal movement, not just execution success. |
| Reconciliation | Compare the book to holdings at the same committed height; do not silently credit a discrepancy. | Independently observed holdings pinned to the cursor digest; reject incomplete history. |

## Two implementations

- **BSC:** fetch a finalized block with full transactions and all receipts. Native BNB uses direct transfers; ERC-20 uses logs from allowlisted contracts. Match payment hash and sender nonce, check the intended transfer and account for actual gas. A separate BNB top-up funds a token sweep; it is not a deposit.
- **Mango:** fetch the entire checkpoint and decode its BCS locally. Compute the supported address-owned coin changes, excluding gas and storage rebates. Match the signed digest and versioned inputs, verify the recipient gain and account for net gas. A sponsored sweep needs both signatures, not a separate gas transfer.

Do not force a Mango object version into an EVM nonce field or model a sponsor
as a BNB transfer. Extend chain evidence and its verification when necessary;
do not implement a second ledger or settle orders in the broadcaster.

## Adding a network

1. Register its network and asset identifiers in `custody-core` and the schema.
   Specify supported transfers, address validation and what is deliberately excluded.
2. Implement ingestion and canonical decoding. Feed the existing ordered
   [pipeline](../crates/custody-core/src/pipeline.rs) and
   [store](../crates/custody-store/src/lib.rs). Supply chain-specific settlement
   evidence alongside receipts; keep raw amounts exact.
3. Implement key handling, transaction planning, signing, rebroadcast and
   fixed-height reconciliation. Wire the processes, append-only asset configuration
   and optional first height into Compose. Never put keys in the reader.
4. Provide deterministic chain fixtures and run the [test suites](TESTING.md).
   A new adapter is not complete just because it can fetch blocks.

## Acceptance cases

| Case | Required observation |
| --- | --- |
| Deposit, interrupt before or after commit, restore config | Same cursor or contiguous successor; exactly one credit; outstanding sweep remains eligible. |
| Add a coin after first use | The last committed height fixes the opening; only the next height can credit it. A stale prefetch cannot commit. |
| Remove or redefine a listed coin | Startup rejects the configuration without advancing the cursor. Restoring all listed entries works without rescanning. |
| Missing height, parent mismatch, incomplete receipts/checkpoint | No ledger change beyond the last verified contiguous height. |
| Broadcast acknowledgement lost | Identical signed payload on retry, no second payment or release of principal. |
| Sweep or sponsor funding seen by scanner | No new user credit; fees booked separately. |
| Mango balance spans several pages | Transfer all objects on a nonempty first page, even if their total value is zero. Do not prepare the next batch until the checkpoint proves delivery to hot. Stop below the gross balance threshold. |
| Fragmented Mango hot wallet | Read at most one 50-coin page per required type. If the page cannot pay but the total can, consolidate the page, wait for checkpoint confirmation, then retry. No second hot-wallet expenditure can overtake it. Only the final payment settles the frozen principal. |
| Finalized sweep fails repeatedly | At most three delayed retries per address. New deposits do not bypass the limit; unknown or conflicting outcomes never authorize a new transaction. |
| Replay an old height after a restart | Verify with the stored historical asset rules; no duplicate deposit, fee or settlement. |

See the [store tests](../crates/custody-store/tests/postgres.rs),
[BSC payment fixtures](../crates/custody-signer/tests/payout.rs) and
[Mango payment fixtures](../mango-worker/tests/payout.rs) for executable examples.

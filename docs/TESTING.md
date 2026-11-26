# Testing

Run the following from the project root. The checks use local fixtures or
disposable databases. They do not require funded wallets or send transactions
to a public chain. First-time builds need internet access to fetch dependencies.

| Command | What it checks |
| --- | --- |
| `cargo test --workspace --locked` | Core logic, the bounded fetch pipeline, BSC receipt decoding and signer unit tests. PostgreSQL integration tests are run separately. |
| `sh scripts/test-postgres.sh` | Ledger posting and replay, wallet allocation, BSC payments, gas funding, sweeps and web flows against an isolated PostgreSQL cluster. Requires PostgreSQL server and client tools on PATH. |
| `sh scripts/test-postgres.sh --mango` | Also runs Mango checkpoint fixtures and sponsored sweeps: an empty page creates no order, a page of zero-balance objects moves to hot before the next batch, the wrong final owner prevents settlement, and internal transfers never credit the user. The [worker build](SCANNERS.md#mango-build-and-inspect-the-worker) fetches its pinned Git dependencies automatically. |
| `sh scripts/test-docker-migrations.sh` | Builds the app and runs its migrations twice against fresh PostgreSQL 17 in Docker. It does not use the Demo's database or keystore. |

To trace a failure, start with the [BSC decoder tests](../crates/evm-reader/tests/scan.rs),
[ledger replay tests](../crates/custody-store/tests/postgres.rs),
[BSC payment tests](../crates/custody-signer/tests/payout.rs) or
[Mango payment tests](../mango-worker/tests/payout.rs).

The [fragmented hot-wallet tests](../mango-worker/tests/payout/fragmentation.rs)
exercise native and token withdrawals from 10,000 small coins, five consecutive
first-page consolidations without following a cursor, a full last page with
insufficient funds, concurrent checkpoint writes, a competing queued withdrawal, lost broadcast acknowledgements,
checkpoint replay and a failed consolidation without releasing the principal.

The [CI workflow](../.github/workflows/acceptance.yml) runs the workspace tests,
PostgreSQL/Mango suite and Docker migration check on pushes and pull requests.
The PostgreSQL script starts its own Unix-socket-only cluster, overrides any
existing database URL and removes only the temporary cluster. Cargo accepts
`--offline` once all required dependencies are cached.

## Live checks

These checks need reachable testnet nodes, unlike the regression suite above.
They do not require test funds.

| Goal | Procedure |
| --- | --- |
| Verify an installation | After the [Docker quickstart](../README.md#docker-quickstart), run `sh scripts/smoke-compose.sh` on an **unfunded** installation. It requests four deposit addresses and checks the four platform wallets. Add `--restart` to recreate workers and verify wallet identity and contiguous scans. |
| Verify wallet isolation | After the smoke test, run `sh scripts/test-compose-installations.sh`. It compares the addresses with a second unfunded Compose installation, then removes only its temporary volumes. |
| Verify persisted scan continuity | Run `sh scripts/test-postgres.sh --live-bsc` or `--live-mango`. Each runs the relevant checks, starts the selected reader twice and verifies that every height between the two cursors was committed. |
| Measure catch-up | Build the Compose images, then run `sh scripts/benchmark-scanners.sh` (Docker Compose, curl, jq and OpenSSL). It starts an isolated database and one reader at a time, with no keys, broadcaster or published ports. |

The benchmark starts 1,200 BSC blocks and 600 Mango checkpoints behind the
sampled finalized heads by default. Override `BSC_BENCH_HEIGHTS` and
`MANGO_BENCH_HEIGHTS` (1 to 5,000). It checks persisted heights and parent links,
reports throughput including retries and samples payload sizes and peak reader
memory when available. It stops at the initially sampled tip; results depend on
the provider and transaction density, not just the reader.

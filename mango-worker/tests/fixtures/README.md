# Checkpoint fixtures

These files use the raw BCS format returned by `GET /rest/checkpoints/{height}/full`.
They are decoder inputs, not seed data. Replay them with
`sh scripts/test-postgres.sh --mango` from the project root.

| Fixture | Regression covered |
| --- | --- |
| `testnet-106658.bcs` | Decode a public Mango Testnet checkpoint using the pinned protocol types. |
| `localnet-two-recipients.bcs` | One transaction can credit two different recipients. |
| `localnet-mixed-assets.bcs` | A different coin with the same symbol is not native MGO. |
| `localnet-split.bcs` | Splitting coins without changing ownership is not a deposit. |
| `localnet-rebate.bcs` | A positive storage rebate alone is not a deposit. |

The localnet inputs are from a disposable network. Tests decode them directly;
the live reader still checks the configured testnet genesis.

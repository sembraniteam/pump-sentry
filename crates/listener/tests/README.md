# listener/tests — frozen mainnet fixtures only

This directory holds **no test targets**: `tests/` contains only
`fixtures/` (frozen mainnet JSON/bin blobs replayed by unit tests via
`include_str!` / `include_bytes!`).

Fixture inventory (each pinned by at least one replay test asserting
wallet attribution + exact base-unit amounts):

- `create_v2_mainnet.json` — `create_v2` launch + one `buy_v2`
  (also reused by the executor `buy_v3`? no — buys only).
- `buy_v2_second_mainnet.json` — second independent `buy_v2`.
- `sell_v2_mainnet.json`, `sell_v2_second_mainnet.json` — two `sell_v2` samples.
- `buy_v3_mainnet.json`, `sell_v3_mainnet.json`,
  `buy_exact_quote_in_v3_mainnet.json` — v3 fills (17-account layout).
- `migrate_v2_pumpswap.json` — `MigrateV2` (PumpSwap) migration.
- `fee_config_live.bin` — live FeeConfig bytes (4097 B). **Single copy**:
  both `executor::quote` and `curve::fee` decode tests include this one
  file by relative path; do not duplicate it into other crates.

Rules: never hand-edit a fixture (re-fetch + re-freeze instead), never
delete one while a replay test includes it, and keep the
`slot`/amount/wallet asserts in the replay tests — they are the proof
the parsers match the live wire shape.

# listener/tests — frozen mainnet fixtures only

This directory holds **no test targets**: `tests/` contains only
`fixtures/` (frozen mainnet JSON/bin blobs replayed by unit tests via
`include_str!` / `include_bytes!`).

Fixture inventory (each pinned by at least one replay test asserting
wallet attribution + exact base-unit amounts):

- `create_v2_mainnet.json` — `create_v2` launch (v1 envelope) + one `buy_v2`.
- `create_v2_v0_recent_mainnet.json` — fresh `create_v2` in a **v0** envelope:
  launches ship both envelopes, so the fetch config must keep allowing both.
- `create_v2_v1_recent_mainnet.json` — fresh `create_v2` in a **v1** envelope.
- `buy_v2_second_mainnet.json` — second independent `buy_v2`.
- `sell_v2_mainnet.json`, `sell_v2_second_mainnet.json` — two `sell_v2` samples.
- `buy_v3_mainnet.json`, `sell_v3_mainnet.json`,
  `buy_exact_quote_in_v3_mainnet.json` — v3 fills (17-account layout).
- `migrate_v2_pumpswap.json` — `MigrateV2` (PumpSwap) migration.
- `fee_config_live.bin` — live FeeConfig bytes (4097 B), shared by
  `executor::quote` and `curve::fee` decode tests (include by relative
  path; do not duplicate it into other crates).
- `amm_global_config_live.bin`, `amm_fee_config_live.bin` — live
  Raydium AMM global/fee-config bytes, included by `executor::quote`
  decode tests.

Rules: never hand-edit a fixture (re-fetch + re-freeze instead), never
delete one while a replay test includes it, and keep the
`slot`/amount/wallet asserts in the replay tests — they are the proof
the parsers match the live wire shape.

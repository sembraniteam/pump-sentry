# listener/tests — frozen mainnet fixtures only

This directory holds **no runnable test targets**: it contains only
frozen mainnet blobs (`fixtures/`, JSON/bin, replayed by unit tests via
`include_str!` / `include_bytes!`) plus these notes.

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
- `buy_18_mainnet.json` — fee-routed 18-account buy (slot 458795810,
  outer): creator/mayhem flow, NOT canonical v3 (fee program +
  buyback wallet + mayhem-linked accounts; trader at index 6, amount =
  first u64). Pinned across 3 samples with the `TradeEvent` user
  cross-checked on each; amount equals the buyer balance delta to the
  unit (`replay_buy_18_amount_matches_balance_delta`).
- `sell_v2_pumpswap_mainnet.json` — PumpSwap `sell_v2` fill (17
  accounts, slot 457835709; Token-2022 base mint, WSOL quote):
  byte-pinned by `sell_pumpswap_v2_matches_frozen_fill`.
- `complete_mayhem_buy_mainnet.json` — router-wrapped `buy_v3`
  (inner 17-account) that completed a MAYHEM curve (slot 458782077):
  carries `TradeEvent` (curve part: 26,616,181,758,971 base units =
  buyer credit exactly) + `CompleteEvent`, no pool part and no
  migrate in the same transaction. Pinned by
  `listener::events::trade_event_pins_curve_part` /
  `complete_event_pins_transition` (decoder lives in
  `crates/listener/src/events.rs`).
- `complete_std_buy_mainnet.json` — same shape on a STANDARD
  (non-mayhem) curve (slot 458796293): `TradeEvent` (20,415,688,021,638
  base units) + `CompleteEvent`, again no pool part in-transaction.
  Pinned by `standard_completion_pins_same_codecs`. Both completions
  landed the curve remainder on EXACTLY 206,900,000,000,000 base units:
  completion is a token floor shared by both curve kinds, not a SOL
  threshold. Recent buys (completing or not) carry a trailing 25th
  data byte (`0x00`/`0x01` both live; historical fills are 24) — an
  evolving wire field, not an exhaust flag — and 24-byte fills keep
  landing, so no builder change is indicated.
- `post_complete_buy_mainnet.json` — Jupiter-routed exact-quote buy
  (slot 458801131) carrying `TradeEvent` + `CompleteEvent` +
  `PostCompleteBuyEvent` (user/mint/curve/timestamp pinned by
  `post_complete_buy_pins_floor_crossing`; tail opaque). The buy pushed
  the remainder BELOW the shared floor (205.8T) — the two on-the-floor
  completions lack this event — yet all bought tokens still came from
  the curve (curve debit == user credit exactly): no pool leg
  materialized in-transaction.
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

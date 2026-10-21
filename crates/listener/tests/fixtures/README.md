# crates/listener/tests/fixtures — gaps (no mainnet fetch here, no fabricated data)

This dir holds frozen mainnet blobs only. Do not hand-write a fixture:
re-fetch + re-freeze instead. This note lists known gaps so a future
operator with RPC access can close them.

## Finding (2026-10-16): launches ship BOTH v0 and v1 envelopes

- Have: `create_v2_mainnet.json` at slot 455573900, version 1.
- New: `create_v2_v0_recent_mainnet.json` at slot 457429472, version **0**
  (sig `2qSAjZs...NPzVPiBGkcHhx`, "BYD AUTO"/"$BYD"), parsed by
  `parser::parse_v2_v0_envelope_recent_launch`.
- Consequence: the old "new launches are version-1 envelopes" shorthand
  is FALSE as a universal — envelope version varies per launch (v0 uses
  address lookup tables; the v0 replay test builds the full key table
  including `loadedAddresses`). The fetch config must keep allowing both:
  `maxSupportedTransactionVersion: 1` covers 0 and 1. Never narrow it to
  v1-only.

## Gap 1 (CLOSED 2026-10-16): newer `create_v2` v1-envelope sample

- New: `create_v2_v1_recent_mainnet.json` at slot 457431003, version 1
  (sig `4oZKh9Jg...`, "TRUMP STREAM "/"TRUMPGOLD", mint
  `E8c671gi75KXEkHWruLQAZFjBLXitqzj5HAfke4cpump`), pinned by
  `parser::parse_v2_v1_envelope_recent_launch` (slot > 455573900,
  version == 1, 16 accounts, name/symbol/mint/curve exact, creator
  present). Hunt evidence: 2 v0 + 1 v1 among the last 3 launches
  resolved — envelope version varies, the fetch config stays
  `maxSupportedTransactionVersion: 1` for both.
- Hunt method (no script kept — re-create as needed): bulk `getBlock`
  scan for the `create_v2` discriminator, then `getTransaction` per hit
  to resolve per-tx versions; freeze the first v1 past the old slot.

## Gap 2: PumpSwap WSOL-quoted executable pool — CLOSED 2026-10-16

Both pool blobs live in `crates/migration/tests/fixtures/`, not in this directory.

- Have (fail-closed pin, keep): `pumpswap_pool_mainnet.json` (`GeWJCT...`) is a depth-2
  pump-coin quote — deliberately non-executable, the trust gate refuses
  it (`is_wsol_quoted() == false` is asserted in
  `crates/migration/src/pumpswap.rs`).
- New (executable proof): `pumpswap_pool_wsol_mainnet.json`
  (`BPZsWwu3hYJXnx9zXhdso83NiaP7EZKR3uKRCHG6geJZ`, base
  `E8ZvySLWzHg2oDBPFyeDYzNzVdN7AA4zTC3tUYs9pump`, quote == WSOL),
  frozen via `getAccountInfo` from sell_v2 flow near tip 457428741,
  pinned by `pumpswap::parses_frozen_wsol_executable_pool`
  (parse + `validate` + `parse_validated` for its base mint, plus
  `via == "getAccountInfo"` provenance).
- Do not mutate the existing depth-2 fixture: it pins the fail-closed
  path.

## Finding (2026-10-19): live-state fixtures decay three ways — re-freeze is a treadmill

- The v3 layout probe starved on public RPC (`largest-accounts`
  throttled + fixture traders flipped/closed ATAs), so a re-freeze was
  attempted — and REVERTED the same day, with evidence:
  1. **Holder churn**: old fixtures' traders sell/close ATAs within
     weeks (`funded_base=0`, `fixture fallback ATA missing on-chain`).
  2. **Graduation**: a fill frozen minutes earlier replayed against a
     graduated curve (`BondingCurveComplete`) — any fixture can die
     between freeze and replay.
  3. **Thin bench + fail-fast**: a young-curve fixture (live, 20
     holders) panicked the probe on its first WSOL-less candidate
     (`insufficient funds`) — `sim_skeleton_green` panics on layout
     verdicts instead of skipping poor candidates, so one poor holder
     aborts the run. A re-freeze that greens needs a MID-curve
     (deep bench, far from graduation): narrow window, same decay.
- Conclusion: do NOT re-freeze on a schedule. Run the layout probes
  against a keyed RPC (indexed methods work → the live
  largest-holders path needs no fresh fixture at all; proven green
  2026-10-19). Re-freeze only if the throttled path must pass on
  public RPC, and expect to redo it within weeks.

## How to freeze (no fabrication)

1. Bulk `getBlock` scan over the target slot range (prefer bulk scans
   over per-signature loops; rate limits bite).
2. `getTransaction` with `maxSupportedTransactionVersion: 1` for tx
   fixtures; `getAccountInfo` for pool fixtures.
3. Store raw RPC `result` (tx) or `{amm, owner, data, via, note}`
   (pool, with slot/sig source in `note`).
4. Add/extend a replay test that asserts slot, wallets, and exact
   base-unit amounts — the test is the proof the parser matches wire.

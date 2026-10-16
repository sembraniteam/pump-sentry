# AGENTS.md — pump-sentry

Paper-trading Solana memecoin sniper (Pump.fun → Raydium). Rust workspace:
binary (`src/`) + 9 crates (`crates/*`). Default mode is paper-only.

## Safety (read first)

- **Never run with `--live` / `IS_LIVE=true`, never fund a wallet, never
  send transactions.** Live paths move real SOL via Jito.
- Tests hitting real mainnet exist and are `#[ignore]`d behind env gates
  (`JITO_ACCEPTANCE=1`, `MAINNET_LAYOUT=1`, `RAYDIUM_ACCEPTANCE=1`).
  Never run them casually;
  only on explicit user request. They cost $0 by construction (unfunded
  payers, simulation-only) — keep it that way. Raydium probes resample
  (router-dominated blocks yield zero direct swaps); rerun, don't "fix".
- No secrets in repo. Preflight/simulate before any dispatch change.

## Verify (in this order)

```sh
cargo test --workspace            # 490 tests; live probes stay ignored
cargo clippy --all-targets        # must be zero warnings
cargo fmt --all -- --check        # repo has PRE-EXISTING diffs: fix only
                                  # your own lines, never whole-tree fmt
```

- Single crate: `cargo test -p <name>`.
- After any behavior change, re-run workspace tests + clippy. Both were
  zero at last commit; keep them zero.

## Architecture that filenames don't tell you

- `crates/ids` is the single source of truth for on-chain constants (program IDs, discriminators, fee/volume addresses).
  A program upgrade
  edits exactly one file there — never duplicate a constant elsewhere.
- `src/pipeline.rs` (~770 lines prod; tests live in `src/pipeline_tests.rs`
  via `#[path]`, same crate so `pub(crate)` stays private) is the orchestrator:
  gate → risk → paper ledger → live mirror. `LiveExecution`/`live_mirror` (live
  dispatch + `FillGapTracker` calibration), `gate` (watchlist + opens), and `DailyBreaker` are split
  out; method bodies live on `Pipeline` via those modules. Do NOT split
  further (coherent as-is; 10 single-impl traits are a deliberate
  testability trade-off, not tech debt to "fix").
- `src/live.rs` sizes/orders live trades; `crates/executor` builds and
  sends them. Paper ledger is authoritative; live legs only mirror it.
  Custody types are `Pubkey`, not `String` (`LivePosition`, `PoolLeg`,
  `LiveBuyParams`/`LiveSellParams`): parse ONCE at the caller boundary
  (`live_buy`, `follow_pool`, WAL replay), never re-validate downstream.
  `String` survives only at RPC/feed boundaries (`ReserveFeed`,
  map keys, logs). `LiveBuyParams` carries explicit `decimals: u8` too.
  Test addresses: `crate::live::TEST_*` consts (+ `test_pubkey(seed)`);
  new test fixtures must use them, never arbitrary strings.
- Paper quantities are `u64` whole tokens (absolute; direction from
  `side`), and the ledger debits/invests the floored size, never the
  nominal ticket. Tests asserting quantities use integers; paper↔live
  size parity holds only to sub-token dust (assert bounded, not exact).
- `paper_dispatch_legs_usd(tip, priority, sol_usd)` takes 3 args (priority
  leg included; calm-weather floor, documented). Typed units live in
  `src/money.rs` (`Bps`/`Percent`) with `Settings::slippage()` /
  `runner_moonbag()` accessors — use them in new validation, don't add
  parallel float checks.
- Risk flow: `risk-engine` profile → `strategy` rewrite (`adaptive`/`scalp`/`runner`) → `simulator` exits
  (TP/SL/trailing/
  timeout/moonbag scale-outs with breakeven ratchet + timeout lift).
- Skills are strong defaults, **not ground truth**:
  the Jito skill once omitted the mandatory `{"encoding":"base64"}`
  param and every bundle was refused (`-32602`). Wire formats must be
  proven against mainnet replays/simulations, never trusted from prose.
  (There is no `.opencode/skills/` dir in this repo; comments naming
  `skill solana-*` refer to out-of-repo docs.)
- Alert burn-in defaults are ON (`alert_diverged_warn_n=3`,
  `alert_risk_fallback_warn_pct=50`, `alert_giveup_burst_n=3`): model
  trouble and divergence page ops by default. Do NOT "fix" them back to 0.
- One shared `Arc<RpcClient>` per process (built in `main`, injected via
  `with_rpc` / `subscribe_with_rpc`). Never add a new `RpcClient`
  construction site; `new()` constructors are test/back-compat wrappers.
- Resend guard holds on `Pending` AND `Unknown` (`prior_sell_pending`);
  the stale "Unknown proceeds" comment in `live_mirror.rs` was fixed —
  do not revert it.
- `venue_fee=None` at confirm fails closed (non-synthetic). Test stubs
  default to `Some(125.0)`; the `None`-fee test arm asserts zero opens.

## Hard-earned mainnet facts (don't re-derive)

- New launches are `create_v2` (disc `[214,144,…]`) in **both v0 and v1
  envelopes** (2 v0 + 1 v1 among recent launches; frozen proofs:
  `create_v2_v0_recent_mainnet.json` + `create_v2_v1_recent_mainnet.json`):
  fetch configs must keep `maxSupportedTransactionVersion: 1`, never
  narrow to v1-only. v0 txs use address lookup tables — key-table code
  must include `loadedAddresses`.
- Dispatch is v3-only (`buy_v3`/`sell_v3`, 17 accounts, fees stay on the
  curve). The legacy 12-account core, 18-account mayhem buy, and
  27/26-account `buy_v2`/`sell_v2` shapes are RETIRED from dispatch
  (deleted, Oct-2026): legacy fills are extinct on mainnet and 18/v2
  fills occur only on old curves, while every new launch is a v3 curve.
  Non-v3 curves fail closed (diverged/manual, no funds move). The
  listener still parses all historical shapes (screening + quorum need
  them); only dispatch builders are gone. Do NOT re-add a retired shape
  without a frozen v3-era sample proving a NEW launch uses it.
- Token program is per-mint (classic SPL vs Token-2022); ATA derivation
  must use it. Ticks price with real mint decimals (cached); the confirm
  gate still drops non-6 mints fail-closed.
- Raydium migration pools are WSOL-**based** (WSOL at slot 400, token at
  432; vault contents prove it) — the old code assumed WSOL-quoted and
  rejected every real pool at both the trust gate and the vault pricer.
  Frozen proofs: `crates/migration/tests/fixtures/raydium_pool_fartcoin.json`
  (canonical Oct-2024 Fartcoin pool) + `raydium_pool_mainnet.json`.
- v2 launches have TWO insider identities (payer `user` + tail `creator`):
  both are screened (worst trust wins) and both join the gate cohort
  (either dump aborts). Prior-launch counting uses the versioned author
  index (v1: 7, v2: 5) — a shared index undercounted v2 serial launchers.
- Sizing takes per-mint `decimals` explicitly (no hidden constant in the
  calc path): the confirm gate verifies decimals per mint and drops
  non-6 fail-closed, and live re-verifies at dispatch. Live dispatch
  preflight is warn-and-proceed unless `SIM_STRICT=true` (strict refuses
  unproven); `SwapExecutor::new` defaults strict ON, paper paths opt
  out explicitly via `with_strict_sim(false)`.
- Fee payer ≠ `user` happens (relayers); holders may sit in aux
  accounts, not the canonical ATA (a live `sell_v2` probe hit exactly
  this: one seller's `[14]` was aux, so the builder's canonical-ATA
  assumption fails closed there — our own holdings are always canonical
  via the idempotent ATA prefix). Simulation with `sig_verify:false`
  proves layout/logic, never authorization.
- Dispatch is Jito-bundles-only (`send_bundle`): no public-mempool
  send exists anywhere (deleted — sniper flow must never face
  sandwich exposure). Do NOT re-add one without a written MEV analysis.
- PumpSwap `sell_v2` IS sim-proven now (`pumpswap_sell_v2_simulates`:
  builder + quote floor vs live pool + live funded holder, GREEN).
  Still no frozen sell_v2 fill fixture, and no funded trial
  (buy-then-sell for real) — that trial is an operator action, never
  run from here.
- Mainnet fixtures live in `crates/listener/tests/fixtures/`; replay
  tests are the gate that must stay green.

## Toolchain quirks

- Mixed Solana versions by design: `executor` builds on granular
  v3/v4 crates (`solana-client 4.3`), not `solana-sdk 5`.
  Don't "unify" them.
- Rust 1.99 (`rust-version` in manifests). Let-chains (`if let … && let`)
  are used and required.
- Long RPC-bound commands: run in background, don't poll; prefer bulk
  `getBlock` scans over per-signature loops (rate limits).
- Commits in this repo are authored as the project owner; if `git commit`
  fails on gpg signing, retry with `-c commit.gpgsign=false`. Never push
  unless asked.

## Working conventions

- Prefer `edit` over new files; derive `oldString` from current content.
- Reference code as `path:line`. Keep responses short, factual, no
  superlatives or emojis. User communicates in Indonesian — answer in
  Indonesian.
- Report honestly: dead ends, failed probes, and rejected hypotheses are
  results. Never fabricate on-chain layouts — mark unverified parts and
  gate them with failing tests.

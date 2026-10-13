# AGENTS.md — solana-snipe-bot

Paper-trading Solana memecoin sniper (Pump.fun → Raydium). Rust workspace:
binary (`src/`) + 9 crates (`crates/*`). Default mode is paper-only.

## Safety (read first)

- **Never run with `--live` / `IS_LIVE=true`, never fund a wallet, never
  send transactions.** Live paths move real SOL via Jito.
- Tests hitting real mainnet exist and are `#[ignore]`d behind env gates (`JITO_ACCEPTANCE=1`, `MAINNET_LAYOUT=1`).
  Never run them casually;
  only on explicit user request. They cost $0 by construction (unfunded
  payers, simulation-only) — keep it that way.
- No secrets in repo. Preflight/simulate before any dispatch change.

## Verify (in this order)

```sh
cargo test --workspace            # 334 tests; live probes stay ignored
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
- `src/pipeline.rs` (~2500 lines) is the orchestrator: gate → risk →
  paper ledger → live mirror. `LiveExecution` and `DailyBreaker` are
  split out; method bodies still live on `Pipeline`.
- `src/live.rs` sizes/orders live trades; `crates/executor` builds and
  sends them. Paper ledger is authoritative; live legs only mirror it.
- Risk flow: `risk-engine` profile → `strategy` rewrite (`adaptive`/`scalp`/`runner`) → `simulator` exits
  (TP/SL/trailing/
  timeout/moonbag scale-outs with breakeven ratchet + timeout lift).
- Skills in `.opencode/skills/` are strong defaults, **not ground truth**:
  the Jito skill once omitted the mandatory `{"encoding":"base64"}`
  param and every bundle was refused (`-32602`). Wire formats must be
  proven against mainnet replays/simulations, never trusted from prose.

## Hard-earned mainnet facts (don't re-derive)

- New launches are `create_v2` (disc `[214,144,…]`), **version-1
  envelopes**: fetch configs must allow v1 or RPC refuses them.
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
- Sizing assumes 6-decimal mints: the confirm gate verifies decimals per
  mint and drops anything else (fail-closed). Live dispatch preflight is
  warn-and-proceed unless `SIM_STRICT=true` (strict refuses unproven).
- Fee payer ≠ `user` happens (relayers); holders may sit in aux
  accounts, not the canonical ATA (a live `sell_v2` probe hit exactly
  this: one seller's `[14]` was aux, so the builder's canonical-ATA
  assumption fails closed there — our own holdings are always canonical
  via the idempotent ATA prefix). Simulation with `sig_verify:false`
  proves layout/logic, never authorization.
- Mainnet fixtures live in `crates/listener/tests/fixtures/`; replay
  tests are the gate that must stay green.

## Toolchain quirks

- Mixed Solana versions by design: `executor` builds on granular
  v3/v4 crates (`solana-client 4.3`), not `solana-sdk 5` (see its README
  version note). Don't "unify" them.
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

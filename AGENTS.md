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
cargo test --workspace            # 219 tests; live probes stay ignored
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
- Swaps come in legacy (`buy`/`sell`) and `buy_v2` (`[184,23,…]`,
  trader at index 13) shapes; `sell_v2` is unknown — dev dumps through
  it are invisible, documented as residual blindness.
- Token program is per-mint (classic SPL vs Token-2022); ATA derivation
  must use it. Fee recipient rotates — resolve from Global bytes
  41..73 (NOT 33; the old doc ignored the Anchor discriminator).
- Fee payer ≠ `user` happens (relayers); holders may sit in aux
  accounts, not the canonical ATA. Simulation with `sig_verify:false`
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

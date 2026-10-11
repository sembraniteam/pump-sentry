# solana-snipe-bot

Paper-trading pipeline for Solana memecoins: listen for new Pump.fun
launches → audit risk via RugCheck → read live bonding-curve reserves →
price TP/SL with an LLM risk engine → simulate fills and auto-exit on
TP/SL. Default mode is paper-only; `--live` adds real Jito-routed
execution (see Live mode).

## Pipeline

```
Pump.fun logsSubscribe (WS)          crates/listener
  → getTransaction → parse create/create_v2 ix → PumpTokenLaunchEvent
  → RugCheck report                  crates/auditor   (reject = skip, fail-closed)
  → dev-wallet history               listener::screen (fresh/serial devs rejected)
  → watchlist: buyer quorum          listener::screen (dev dump aborts, silence times out)
  → bundle forensics at confirm      listener::screen (reject drops, suspect paper-only)
  → bonding-curve reserves           crates/curve     (live price + liquidity)
  → TP/SL profile                    crates/risk-engine (LLM ≤300ms, else heuristic)
  → strategy shape                   STRATEGY_MODE (adaptive/scalp/runner)
  → paper position                   crates/simulator ($100 virtual, slippage-aware)
  → ticker loop polls curves → TP/SL/trailing/timeout auto-close
  → graduation → migration hunt → Raydium pool parse + Discord card
  → post-migration vault ticker → 2x spike alerts (paper tracking)
```

## Crates

| Crate                | Role                                                                                                                 |
|----------------------|----------------------------------------------------------------------------------------------------------------------|
| `crates/listener`    | Pump.fun `create` streaming via `logsSubscribe` → `getTransaction` → Borsh parse; bounded channel, reconnect backoff |
| `crates/auditor`     | RugCheck.xyz client (timeout + retries); policy: score cap 400, no `danger`, no mint/freeze authority                |
| `crates/curve`       | Bonding-curve reserve reader + polling ticker; live USD spot and liquidity from virtual reserves                     |
| `crates/risk-engine` | LLM TP/SL (OpenAI-compatible chat, JSON mode, temp 0.1) with deterministic heuristic fallback after 300ms            |
| `crates/ids`         | Single source of truth: program IDs, discriminators, fee/volume constants (compiler-linked, one edit point)          |
| `crates/simulator`   | Virtual portfolio, bonding-curve slippage fills, TP/SL/trailing/timeout exits, realized + mark-to-market PnL         |
| `crates/executor`    | Live buy/sell (guarded wallet, exact ix builders, Jito bundles) — only active with `--live`                          |
| `crates/alert`       | Discord trade cards via Hermes CLI (fire-and-forget detached tasks, warn-only failures)                              |
| `crates/migration`   | Graduation + Raydium/PumpSwap migration detection, fresh pool parsing                                                |

Each crate has its own README with architecture details and examples.

## Prerequisites

- Rust 1.99+ (`rustup show` to verify)
- Solana mainnet RPC access (default endpoints work, a dedicated RPC is
  recommended for `logsSubscribe` stability)
- Optional: local inference server (Ollama-compatible) for the risk engine;
  without it the heuristic fallback drives decisions

## Quickstart

```sh
# Build everything
cargo build --workspace

# Watch live Pump.fun launches (prints parsed create events)
cargo run -p listener --example watch

# Run the paper pipeline (defaults: $100 virtual, confirmed commitment)
cargo run

# Run with real funds: every BUY signal also dispatches a Jito bundle,
# every paper close also sells the tracked live amount
IS_LIVE=true SOLANA_PRIVATE_KEY=<base58> cargo run -- --live
```

With knobs:

```sh
WS_URL="wss://api.mainnet-beta.solana.com" \
HTTP_URL="https://api.mainnet-beta.solana.com" \
COMMITMENT="confirmed" \
START_BALANCE="100" FEE_BPS="30" NOTIONAL_USD="10" TRAIL_PERCENT="5" \
INFERENCE_ENDPOINT="http://127.0.0.1:11434/v1/chat/completions" \
INFERENCE_MODEL="llama3.2:1b" INFERENCE_TIMEOUT_MS="300" \
QUOTE_USD="0.000001" \
SOL_USD="150" TICK_INTERVAL_MS="1000" \
cargo run
```

| Env                                                                     | Default                       | Meaning                                                                            |
|-------------------------------------------------------------------------|-------------------------------|------------------------------------------------------------------------------------|
| `WS_URL` / `HTTP_URL`                                                   | mainnet-beta                  | RPC endpoints                                                                      |
| `COMMITMENT`                                                            | `confirmed`                   | `processed` \| `confirmed` \| `finalized`                                          |
| `START_BALANCE`                                                         | `100`                         | Virtual USD balance                                                                |
| `FEE_BPS`                                                               | `30`                          | Commission per side (bps)                                                          |
| `NOTIONAL_USD`                                                          | `10`                          | Paper size per trade                                                               |
| `TRAIL_PERCENT`                                                         | `5`                           | Trailing-stop giveback (%)                                                         |
| `MAX_OPEN_POSITIONS` / `MINT_COOLDOWN_SECS`                             | `3` / `3600`                  | Concurrent-position cap; per-mint re-entry cooldown                                |
| `QUOTE_USD`                                                             | *(unset)*                     | Entry-quote override for tests; unset = reserve read or skip                       |
| `GATE_WINDOW_SECS` / `GATE_MIN_BUYERS` / `GATE_POLL_MS`                 | `10` / `5` / `2000`           | Watchlist window, distinct-buyer quorum, observation cadence                       |
| `DEV_MIN_AGE_SECS` / `DEV_MAX_PRIOR_LAUNCHES` / `DEV_SCREEN_SAMPLE_TXS` | `3600` / `1` / `20`           | Dev-wallet minimum age, tolerated prior launches, sampled transactions             |
| `LIQUIDITY_USD`                                                         | `0`                           | Liquidity fallback when the reserve read fails (normally live from reserves)       |
| `DISCORD_ALERTS`                                                        | `true`                        | Trade cards via `hermes send --to discord`; `false` silences                       |
| `SOL_USD` / `TICK_INTERVAL_MS`                                          | `150` / `1000`                | Curve pricing conversion / exit-poll interval                                      |
| `INFERENCE_ENDPOINT` / `INFERENCE_MODEL` / `INFERENCE_TIMEOUT_MS`       | local / `llama3.2:1b` / `300` | Risk-engine inference (OpenAI-compatible; Hermes Agent ready)                      |
| `INFERENCE_API_KEY`                                                     | *(unset)*                     | Bearer token for providers that require auth                                       |
| `IS_LIVE` / `--live`                                                    | *(unset)*                     | Live mode: mirror signals with real Jito-routed swaps (needs `SOLANA_PRIVATE_KEY`) |
| `SOLANA_PRIVATE_KEY`                                                    | *(unset)*                     | Trading keypair, base58 (live mode refuses to start without it)                    |
| `SLIPPAGE_BPS` / `JITO_TIP_LAMPORTS` / `JITO_ENDPOINT`                  | `200` / `100000` / frankfurt  | Live sizing guard, per-bundle tip, block-engine region                             |
| `STRATEGY_MODE` / `RUNNER_MOONBAG_PERCENT` / `RUNNER_TRAIL_PERCENT`     | `adaptive` / `20` / `20`      | Execution shape; runner moonbag share + wide trail (%)                             |
| `MAX_DAILY_LOSS_USD`                                                    | `0` (off)                     | Day-loss circuit breaker: halts new entries past the cap; exits always work        |
| `DISCORD_CHANNEL`                                                       | *(unset)*                     | Hermes `--to discord:<id>` target; unset = host-configured channel                 |
| `HERMES_BIN`                                                            | `hermes`                      | Hermes binary path; prefer absolute (bare name resolves via `PATH`)                |
| `PRIORITY_FEE_MICRO_LAMPORTS` / `COMPUTE_UNIT_LIMIT`                    | `0` / `0`                     | Live compute budget (0 = off); priority price + unit limit                         |
| `FEE_RECIPIENT`                                                         | *(unset)*                     | Override swap fee-recipient; unset = live Global account read                      |

## Live mode

Without flags the bot is a pure simulator. With `--live` (or
`IS_LIVE=true`) each paper open additionally sends a buy bundle (swap + tip in one transaction) and each paper close
sells the tracked
amount — the paper book stays the system of record while live legs are
best-effort with loud errors. Live startup waits 5s before dispatching
(Ctrl-C aborts; automation-safe, no prompt). Sizing derives from the USD notional at
curve spot with a `SLIPPAGE_BPS` guard; sells floor proceeds the same
way. Start on devnet-sized notionals and watch bundle UUIDs in logs.

## Strategies

`STRATEGY_MODE` selects the execution shape (default `adaptive` —
the risk profile runs verbatim):

- `scalp` — quick exits: TP above +35% is cut into the band, full
  exit, no runners. A TP already below +25% is honored as-is (with a
  warning) — widening a model's early exit is never done silently.
- `runner` — moonbag: at TP sells `(100 - RUNNER_MOONBAG)%`
  (default 20 → sells 80%), arms the trailing stop, ratchets the
  runner to breakeven, and lifts the time-stop so the remaining 20%
  rides toward 5x–10x instead of expiring. The runner trails wide (`RUNNER_TRAIL_PERCENT`, default 20%) so normal
  pullbacks don't shake
  it out — the shared 5% `TRAIL_PERCENT` would.

## Testing

```sh
cargo test --workspace                    # 219 tests across all crates, live probes ignored by default
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## Layout

```
src/main.rs            # thin binary: wiring + run loop (+ --live gate)
src/settings.rs        # env knobs (tested)
src/pipeline.rs        # launch handling + tick exits (tested)
src/live.rs            # live sizing + Jito dispatch sink (tested)
crates/curve/          # bonding-curve reserves + price ticker
crates/listener/       # WS event ingestion + Pump.fun parser
crates/auditor/        # RugCheck client + policy
crates/risk-engine/    # prompt schema + LLM/fallback engine
crates/simulator/      # portfolio + slippage + PnL
crates/executor/       # live swaps via Jito (only with --live)
crates/ids/            # single-source on-chain constants
crates/migration/       # graduation detector + Raydium pool parser
```

## Safety notes

- Default mode **cannot spend funds**: without `--live` / `IS_LIVE=true`
  the binary only mutates virtual cash. Live mode moves real SOL through
  Jito bundles — enable it only with a dedicated wallet, small
  `NOTIONAL_USD`, and reviewed parameters.
- `crates/executor` refuses to build anything when the wallet guard
  fails: missing key, or balance below trade + tip + 0.05 SOL reserve.
- Auditing and dev screening are fail-closed (transport errors skip the
  token). Velocity below 5 measured buyers resolves to `PASS` — no demand,
  no trade.
- Entry gating: signals sit on a 10s watchlist until 5 distinct wallets
  buy; dev dumps abort immediately, silence times out. Nothing is bought
  on the create block anymore.
- Bundle forensics is two-tier: slot clustering decides free from gate
  swaps; funder walks (cached, concurrent, deadline-bound) run only when
  slots are inconclusive. Reject drops, suspect trades paper-only.
- Live fills are tracked only after on-chain confirmation: timed-out
  buys become pending and are re-polled (never assumed unfilled, never
  recorded as fills); failed sells stay tracked with cooldown + attempt
  budget, then give up loudly for manual exit.
- Portfolio heat: max 3 concurrent positions, one holding per mint, 1h
  per-mint cooldown after a close.
- Day-loss breaker (`MAX_DAILY_LOSS_USD`): past the cap no new entries
  open (paper or live); exits and live-sell retries keep working. Live
  mode without a cap logs a startup warning.

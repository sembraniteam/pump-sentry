# solana-snipe-bot

Paper-trading pipeline for Solana memecoins: listen for new Pump.fun
launches → audit risk via RugCheck → read live bonding-curve reserves →
price TP/SL with an LLM risk engine → simulate fills and auto-exit on
TP/SL. Default mode is paper-only; `--live` adds real Jito-routed
execution (see Live mode).

## Pipeline

```
Pump.fun logsSubscribe (WS)          crates/listener
  → getTransaction → parse create ix → PumpTokenLaunchEvent
  → RugCheck report                  crates/auditor   (reject = skip, fail-closed)
  → dev-wallet history               listener::screen (fresh/serial devs rejected)
  → watchlist: buyer quorum          listener::screen (dev dump aborts, silence times out)
  → bundle forensics at confirm      listener::screen (reject drops, suspect paper-only)
  → bonding-curve reserves           crates/curve     (live price + liquidity)
  → TP/SL profile                    crates/risk-engine (LLM ≤300ms, else heuristic)
  → paper position                   crates/simulator ($100 virtual, slippage-aware)
  → ticker loop polls curves → TP/SL/trailing/timeout auto-close
```

## Crates

| Crate                | Role                                                                                                                 |
|----------------------|----------------------------------------------------------------------------------------------------------------------|
| `crates/listener`    | Pump.fun `create` streaming via `logsSubscribe` → `getTransaction` → Borsh parse; bounded channel, reconnect backoff |
| `crates/auditor`     | RugCheck.xyz client (timeout + retries); policy: score cap 400, no `danger`, no mint/freeze authority                |
| `crates/curve`       | Bonding-curve reserve reader + polling ticker; live USD spot and liquidity from virtual reserves                     |
| `crates/risk-engine` | LLM TP/SL (OpenAI-compatible chat, JSON mode, temp 0.1) with deterministic heuristic fallback after 300ms            |
| `crates/simulator`   | Virtual portfolio, bonding-curve slippage fills, TP/SL/trailing/timeout exits, realized + mark-to-market PnL         |
| `crates/executor`    | Live buy/sell (guarded wallet, exact ix builders, Jito bundles) — only active with `--live`                          |
| `crates/alert`       | Discord trade cards via Hermes CLI (fire-and-forget detached tasks, warn-only failures)                          |

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
| `QUOTE_USD`                                                             | *(unset)*                     | Entry-quote override for tests; unset = reserve read or skip                       |
| `GATE_WINDOW_SECS` / `GATE_MIN_BUYERS` / `GATE_POLL_MS`                 | `10` / `5` / `2000`           | Watchlist window, distinct-buyer quorum, observation cadence                       |
| `DEV_MIN_AGE_SECS` / `DEV_MAX_PRIOR_LAUNCHES` / `DEV_SCREEN_SAMPLE_TXS` | `3600` / `1` / `20`           | Dev-wallet minimum age, tolerated prior launches, sampled transactions             |
| `LIQUIDITY_USD`                                                         | `0`                           | Liquidity fallback when the reserve read fails (normally live from reserves)       |
| `DISCORD_ALERTS`                                                      | `true`                        | Trade cards via `hermes send --to discord`; `false` silences                                             |
| `SOL_USD` / `TICK_INTERVAL_MS`                                          | `150` / `1000`                | Curve pricing conversion / exit-poll interval                                      |
| `INFERENCE_ENDPOINT` / `INFERENCE_MODEL` / `INFERENCE_TIMEOUT_MS`       | local / `llama3.2:1b` / `300` | Risk-engine inference (OpenAI-compatible; Hermes Agent ready)                      |
| `INFERENCE_API_KEY`                                                     | *(unset)*                     | Bearer token for providers that require auth                                       |
| `IS_LIVE` / `--live`                                                    | *(unset)*                     | Live mode: mirror signals with real Jito-routed swaps (needs `SOLANA_PRIVATE_KEY`) |
| `SOLANA_PRIVATE_KEY`                                                    | *(unset)*                     | Trading keypair, base58 (live mode refuses to start without it)                    |
| `SLIPPAGE_BPS` / `JITO_TIP_LAMPORTS` / `JITO_ENDPOINT`                  | `200` / `100000` / frankfurt  | Live sizing guard, per-bundle tip, block-engine region                             |

## Live mode

Without flags the bot is a pure simulator. With `--live` (or
`IS_LIVE=true`) each paper open additionally sends a buy bundle (swap + tip in one transaction) and each paper close
sells the tracked
amount — the paper book stays the system of record while live legs are
best-effort with loud errors. Sizing derives from the USD notional at
curve spot with a `SLIPPAGE_BPS` guard; sells floor proceeds the same
way. Start on devnet-sized notionals and watch bundle UUIDs in logs.

## Testing

```sh
cargo test --workspace                    # 155 tests: 4 alert, 17 auditor, 9 curve, 14 executor, 31 listener, 20 risk-engine, 21 simulator, 39 binary
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

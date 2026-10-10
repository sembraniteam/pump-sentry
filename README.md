# solana-snipe-bot

Paper-trading pipeline for Solana memecoins: listen for new Pump.fun
launches → audit risk via RugCheck → read live bonding-curve reserves →
price TP/SL with an LLM risk engine → simulate fills and auto-exit on
TP/SL. No real funds move; execution is out of scope by design.

## Pipeline

```
Pump.fun logsSubscribe (WS)          crates/listener
  → getTransaction → parse create ix → PumpTokenLaunchEvent
  → RugCheck report                  crates/auditor   (reject = skip, fail-closed)
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

| Env                                                               | Default                       | Meaning                                                                             |
|-------------------------------------------------------------------|-------------------------------|-------------------------------------------------------------------------------------|
| `WS_URL` / `HTTP_URL`                                             | mainnet-beta                  | RPC endpoints                                                                       |
| `COMMITMENT`                                                      | `confirmed`                   | `processed` \| `confirmed` \| `finalized`                                           |
| `START_BALANCE`                                                   | `100`                         | Virtual USD balance                                                                 |
| `FEE_BPS`                                                         | `30`                          | Commission per side (bps)                                                           |
| `NOTIONAL_USD`                                                    | `10`                          | Paper size per trade                                                                |
| `TRAIL_PERCENT`                                                   | `5`                           | Trailing-stop giveback (%)                                                          |
| `QUOTE_USD`                                                       | *(unset)*                     | Entry-quote override for tests; unset = reserve read or skip                        |
| `LIQUIDITY_USD` / `BUY_VELOCITY_5S` / `DEV_REPUTATION`            | `0` / `0` / `0.0`             | Fallbacks: liquidity now live from reserves; velocity/dev still stubbed (PASS-safe) |
| `SOL_USD` / `TICK_INTERVAL_MS`                                    | `150` / `1000`                | Curve pricing conversion / exit-poll interval                                       |
| `INFERENCE_ENDPOINT` / `INFERENCE_MODEL` / `INFERENCE_TIMEOUT_MS` | local / `llama3.2:1b` / `300` | Risk-engine inference                                                               |

## Testing

```sh
cargo test --workspace                    # 91 tests: 17 auditor, 9 curve, 14 listener, 19 risk-engine, 15 simulator, 17 binary
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## Layout

```
src/main.rs            # thin binary: wiring + run loop
src/settings.rs        # env knobs (tested)
src/pipeline.rs        # launch handling + tick exits (tested)
crates/curve/          # bonding-curve reserves + price ticker
crates/listener/       # WS event ingestion + Pump.fun parser
crates/auditor/        # RugCheck client + policy
crates/risk-engine/    # prompt schema + LLM/fallback engine
crates/simulator/      # portfolio + slippage + PnL
```

## Safety notes

- This bot **cannot spend funds**: the simulator only mutates virtual cash.
- Auditing is fail-closed (transport errors skip the token), and unknown
  metrics default to `PASS`.
- `main.rs` marks the remaining gaps explicitly (`TODO(live-feeds)`,
  `TODO(price-feed)`): real liquidity/velocity/reputation feeds and a
  quote stream with exit polling are the next build steps.

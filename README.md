# pump-sentry

Paper-trading Solana memecoin sniper: catches new Pump.fun launches → screens risk → paper-trades with auto-exits. Paper-only by default; `--live` adds real Jito-routed execution.

Flow: `logsSubscribe` → RugCheck audit → dev/creator screen → buyer-quorum watchlist → bundle forensics → TP/SL profile (LLM ≤300ms, else heuristic) → paper position → auto-exit → migration hunt → Raydium tracking.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/sembraniteam/pump-sentry/main/install.sh | bash
# VERSION=vX.Y.Z bash install.sh | bash install.sh --from-source | bash uninstall.sh [--purge]
```

Needs: mainnet RPC (dedicated recommended for `logsSubscribe`); Rust 1.99+ for source builds only; optional Ollama-compatible inference server (else heuristic fallback).

## Run

```sh
pump-sentry start            # daemon, log ~/.pump-sentry/bot.log
pump-sentry status|logs|stop # inspect / tail (+--follow) / stop with drain+snapshot
pump-sentry restart --live   # live mode (needs SOLANA_PRIVATE_KEY)
pump-sentry sell <mint>      # manual full exit, no restart
pump-sentry run              # foreground (debugging)
```

State lives in `~/.pump-sentry/` (`bot.log`, `bot.pid`, sell requests, WAL, snapshots); `LOG_FILE`/`PID_FILE` override it. Long runs need `ops/logrotate/pump-sentry` (rename + SIGHUP, lossless).

Live funding: v3 buys debit the wallet **WSOL** ATA, never native SOL — keep a WSOL float for inventory **and** native SOL for fees/tips/0.05 reserve. SOL-only wallets fail every buy (`spl-token wrap` first).

## Config

| Env                                                                     | Default                       | Meaning                                               |
|-------------------------------------------------------------------------|-------------------------------|-------------------------------------------------------|
| `MAINNET_WSS_RPC_URL` / `MAINNET_RPC_URL`                               | mainnet-beta                  | RPC endpoints                                         |
| `COMMITMENT`                                                            | `confirmed`                   | `processed` refused in live mode                      |
| `START_BALANCE` / `FEE_BPS` / `NOTIONAL_USD`                            | `100` / `30` / `10`           | Virtual cash, fee/side (bps), size/trade (USD)        |
| `TRAIL_PERCENT`                                                         | `5`                           | Trailing-stop giveback (%)                            |
| `MAX_OPEN_POSITIONS` / `MINT_COOLDOWN_SECS`                             | `3` / `3600`                  | Exposure cap, per-mint re-entry cooldown              |
| `GATE_WINDOW_SECS` / `GATE_MIN_BUYERS` / `GATE_POLL_MS`                 | `10` / `5` / `2000`           | Watchlist window, quorum, poll cadence                |
| `DEV_MIN_AGE_SECS` / `DEV_MAX_PRIOR_LAUNCHES` / `DEV_SCREEN_SAMPLE_TXS` | `3600` / `1` / `20`           | Min wallet age, tolerated prior launches, sample size |
| `TICK_INTERVAL_MS` / `VAULT_TICK_INTERVAL_MS`                           | `1000` / `5000`               | Curve / pool poll intervals                           |
| `INFERENCE_ENDPOINT` / `INFERENCE_MODEL` / `INFERENCE_TIMEOUT_MS`       | local / `llama3.2:1b` / `300` | Risk-engine LLM (OpenAI-compatible)                   |
| `STRATEGY_MODE` / `RUNNER_MOONBAG_PERCENT` / `RUNNER_TRAIL_PERCENT`     | `adaptive` / `20` / `20`      | `adaptive`/`scalp`/`runner` + runner shape            |
| `MAX_DAILY_LOSS_USD`                                                    | `0` (off)                     | Halts new entries past the cap; exits always work     |
| `SLIPPAGE_BPS` / `JITO_TIP_LAMPORTS` / `JITO_ENDPOINT`                  | `200` / `100000` / frankfurt  | Buy guard, per-bundle tip, block-engine region        |
| `PRIORITY_FEE_MICRO_LAMPORTS` / `COMPUTE_UNIT_LIMIT`                    | `0` / `0`                     | Compute budget (0 = off)                              |
| `SIM_STRICT`                                                            | `false`                       | Unproven sims refuse dispatch (required live)         |
| `QUOTE_USD` / `LIQUIDITY_USD`                                           | *(unset)* / `0`               | Test-only price override / reserve fallback           |
| `IS_LIVE` / `SOLANA_PRIVATE_KEY`                                        | *(unset)*                     | Live mode + trading keypair (base58)                  |
| `DISCORD_ALERTS` / `DISCORD_CHANNEL` / `HERMES_BIN`                     | `true` / *(unset)* / `hermes` | Trade cards via Hermes CLI                            |
| `INFERENCE_API_KEY`                                                     | *(unset)*                     | Bearer token when the provider needs one              |

`STRATEGY_MODE`: `adaptive` runs the profile verbatim; `scalp` clamps TP into +25…+35% (sub-band honored, never widened); `runner` sells 80%, trails the 20% moonbag wide toward 5x–10x.

## Safety

- Paper-only without `--live`/`IS_LIVE=true`. Live needs a dedicated wallet, small `NOTIONAL_USD`, `MAX_DAILY_LOSS_USD>0`, `SIM_STRICT=true`.
- Fail-closed everywhere: audit rejects, fresh/serial insiders, non-6-decimal mints, no-quorum timeouts, dev dumps (incl. `sell_v2`), bundled supply monopolies, unconfirmed fills (re-polled, never assumed).
- Exits never block: TP/SL/trailing/timeout + manual `sell`; breaker halts entries only.

## Layout

```
src/               # cli (supervisor) / settings / pipeline (orchestrator) / live (sizing+dispatch)
crates/listener/   # WS ingestion + parser + screening + quorum gate
crates/executor/   # live swaps via Jito (only with --live)
crates/curve|auditor|risk-engine|simulator|ids|alert|migration/
```

Each crate has its own README. Tests: `cargo test --workspace` (339, live probes ignored) · `cargo clippy --all-targets` (zero warnings).

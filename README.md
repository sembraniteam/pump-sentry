# pump-sentry

Paper-trading sniper for new Pump.fun launches. Paper-only by default; `--live` adds real Jito-routed execution.

Flow: `logsSubscribe` → RugCheck audit → dev screen → buyer quorum → bundle forensics → TP/SL profile (LLM ≤300ms, else heuristic) → paper position → auto-exit → migration hunt → pool tracking.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/sembraniteam/pump-sentry/main/install.sh | bash
# VERSION=vX.Y.Z bash install.sh | bash install.sh --from-source | bash uninstall.sh [--purge]
```

Needs: mainnet RPC (dedicated recommended); Rust 1.99+ for source builds; optional Ollama-compatible server (else heuristic).

## Run

```sh
pump-sentry start            # daemon, log ~/.pump-sentry/bot.log
pump-sentry status|logs|stop # inspect / tail (+--follow) / stop with drain+snapshot
pump-sentry restart --live   # live mode (needs SOLANA_PRIVATE_KEY)
pump-sentry sell <mint>      # manual full exit, no restart
pump-sentry run              # foreground (debugging)
pump-sentry upgrade          # install latest release (says so when behind nothing)
```

State lives in `~/.pump-sentry/` (`bot.log`, `bot.pid`, sell requests, WAL, snapshots); `LOG_FILE`/`PID_FILE` override it. Long runs need `ops/logrotate/pump-sentry`.

Live funding: SOL-pair v3 buys pay **native SOL** — fund native for trade + fees + tips + 0.05 reserve. A **WSOL** float is only needed for token-paired quotes.

## Config

`~/.pump-sentry/config.toml` (auto-created on first start; `pump-sentry init-config`, `CONFIG_FILE` overrides the path), grouped by category.
This file is the **only source for tunables** — env vars are not read for them.
Secrets (`SOLANA_PRIVATE_KEY`, `INFERENCE_API_KEY`) stay in env, never in the file.

| Category    | Keys                                                                                                                                                                              | Defaults                                                    |
|-------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-------------------------------------------------------------|
| `network`   | `mainnet_rpc_url`, `mainnet_wss_rpc_url`, `yellowstone_endpoint`*, `commitment`, `tick_interval_ms`, `vault_tick_interval_ms`                                                    | public RPC / `confirmed` / `1000` / `5000`                  |
| `trading`   | `start_balance`, `fee_bps`, `notional_usd`, `max_open_positions`, `mint_cooldown_secs`, `max_daily_loss_usd`, `quote_usd`*, `liquidity_usd`                                       | `100` / `0` / `10` / `3` / `3600` / `0` (off) / unset / `0` |
| `exits`     | `trail_percent`, `runner_trail_percent`                                                                                                                                           | `5` / `20`                                                  |
| `gate`      | `gate_window_secs`, `gate_min_buyers`, `gate_poll_ms`, `dev_min_age_secs`, `dev_max_prior_launches`, `dev_screen_sample_txs`, `dev_sig_limit`                                     | `10` / `5` / `2000` / `3600` / `1` / `20` / `100`           |
| `risk`      | `strategy_mode`, `runner_moonbag_percent`, `inference_endpoint`, `inference_model`, `inference_timeout_ms`, `speculative_inference`                                               | `adaptive` / `20` / local / `llama3.2:1b` / `300` / `false` |
| `execution` | `slippage_bps`, `slippage_vol_window_secs`, `slippage_vol_extra_cap_bps`, `priority_fee_micro_lamports`, `compute_unit_limit`, `jito_tip_lamports`, `jito_endpoint`, `sim_strict` | `200` / `0` / `0` / `0` / `0` / `100000` / global / `false` |
| `alerts`    | `discord_alerts`, `discord_channel`, `alert_diverged_warn_n`, `alert_risk_fallback_warn_pct`, `alert_giveup_burst_n`, `alert_giveup_window_secs`                                  | `true` / host default / `0` / `0` / `0` / `3600`            |

\* test-only. \*`yellowstone_endpoint`: non-empty selects Yellowstone gRPC ingestion instead of WS (x-token via `YELLOWSTONE_TOKEN` env, never the file).
`strategy_mode`: `adaptive` runs the profile verbatim; `scalp` clamps TP to +25…+35%; `runner` sells 80%, trails the 20% moonbag toward 5x–10x.

Outside config: `HERMES_BIN`, `BOT_DAEMON` (daemon marker), `PID_FILE` / `LOG_FILE` / `CONFIG_FILE` (path overrides), `YELLOWSTONE_TOKEN` (geyser x-token, secret).

## Safety

- Paper-only without `--live`/`IS_LIVE=true`. Live needs a dedicated wallet, small `notional_usd`, `max_daily_loss_usd>0`, `sim_strict=true`.
- Fail-closed everywhere: audit rejects, fresh/serial insiders, non-6-decimal mints, no-quorum timeouts, dev dumps, bundled supply monopolies, unconfirmed fills (re-polled, never assumed).
- Post-migration exits route through the validated pool leg (Token-2022 vaults refuse fail-closed).
- Exits never block: TP/SL/trailing/timeout + manual `sell`; the day-loss breaker halts entries only.

## Layout

```
src/               # cli (supervisor) / settings / pipeline (orchestrator) / live (sizing+dispatch)
                   # gate (watchlist+opens) / live_mirror (live dispatch) / follow (migration watch) / wal (custody log)
crates/listener/   # WS ingestion + parser + screening + quorum gate
crates/executor/   # live swaps via Jito (only with --live)
crates/curve|auditor|risk-engine|simulator|ids|alert|migration/
```

Each crate has its own README. Tests: `cargo test --workspace` (live probes ignored) · `cargo clippy --all-targets` (zero warnings).

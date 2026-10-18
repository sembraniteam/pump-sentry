# pump-sentry

Paper-trading sniper for new Pump.fun launches.

It watches every new launch, throws away scams and weak setups through
several filters, opens **virtual (paper) positions**, and exits them
automatically. No real money moves unless you explicitly enable live
mode (see [Going live](#going-live)).

Pipeline: `logsSubscribe` → RugCheck audit → dev screen → buyer quorum → bundle forensics → TP/SL profile (LLM ≤300ms, else heuristic) → paper position → auto-exit → migration hunt → pool tracking.

## Quickstart (paper mode, no money involved)

1. **Install.** You need a mainnet RPC (dedicated recommended); Rust 1.99+ only matters for source builds.

   ```sh
   curl -fsSL https://raw.githubusercontent.com/sembraniteam/pump-sentry/main/install.sh | bash
   # VERSION=vX.Y.Z bash install.sh | bash install.sh --from-source | bash uninstall.sh [--purge]
   ```

2. **Create the config.** `pump-sentry init-config` writes `~/.pump-sentry/config.toml` (also auto-created on first start). At minimum, set your `mainnet_rpc_url`.
3. **Run it.** `pump-sentry run` for a first look (foreground), or `pump-sentry start` (daemon).
4. **Watch it.** `pump-sentry logs` and `pump-sentry status`. Let it run **1–2 weeks** and read the heartbeat lines (diverged count, fallback rate, execution gap, watchlist→open ratio) before even thinking about live mode.
5. **LLM is optional.** Point `[risk]` at an Ollama-compatible server for model-driven exits; without one, the deterministic heuristic decides and paper trading still works.

## Commands

| Command                    | Effect                                                        |
|----------------------------|---------------------------------------------------------------|
| `pump-sentry start`        | run as daemon; logs to `~/.pump-sentry/bot.log`               |
| `pump-sentry status`       | inspect daemon state                                          |
| `pump-sentry logs`         | tail the log (`--follow` to follow)                           |
| `pump-sentry stop`         | stop with drain + snapshot                                    |
| `pump-sentry restart --live` | restart in live mode (needs `SOLANA_PRIVATE_KEY`)           |
| `pump-sentry sell <mint>`  | manual full exit, no restart                                  |
| `pump-sentry pause`        | kill switch: halt new entries; open positions keep managing   |
| `pump-sentry resume`       | clear the kill switch                                         |
| `pump-sentry run`          | foreground (debugging)                                        |
| `pump-sentry upgrade`      | install latest release                                        |
| `pump-sentry init-config`  | write the default `config.toml` (`CONFIG_FILE` overrides path) |

State lives in `~/.pump-sentry/` (`bot.log`, `bot.pid`, sell requests, WAL, day-state, kill-switch marker, snapshots); `LOG_FILE`/`PID_FILE` override it. Long runs need `ops/logrotate/pump-sentry`.

Live funding: SOL-pair v3 buys pay **native SOL** — fund native for trade + fees + tips + 0.05 reserve. A **WSOL** float is only needed for token-paired quotes.

## Uninstall

```sh
pump-sentry stop            # stop the daemon first (the script won't)
curl -fsSL https://raw.githubusercontent.com/sembraniteam/pump-sentry/main/uninstall.sh | bash
# curl -fsSL .../uninstall.sh | bash -s -- --purge   # also remove ~/.pump-sentry (config, logs, WAL, snapshots)
```

The script removes the binary from `/usr/local/bin`, `~/.local/bin`, and `~/.pump-sentry/bin` (`INSTALL_DIR` overrides the search path). It never touches your shell profile: unset `SOLANA_PRIVATE_KEY` / `INFERENCE_API_KEY` / RPC URL vars there yourself, and delete the repo clone if you built from source. Reinstalling without `--purge` resumes from the kept state — the WAL replays live custody on next live boot, so verify bags on-chain first (see `ops/RUNBOOK.md` §4).

## Config

`~/.pump-sentry/config.toml`, grouped by `[section]` — one key per line.
This file is the **only source for tunables** — env vars are not read for them.
Secrets (`SOLANA_PRIVATE_KEY`, `INFERENCE_API_KEY`) stay in env, never in the file.

| Section     | Keys                                                                                                                                                                                                 | Defaults                                                           |
|-------------|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|--------------------------------------------------------------------|
| `network`   | `mainnet_rpc_url`, `mainnet_wss_rpc_url`, `yellowstone_endpoint`*, `commitment`, `tick_interval_ms`, `vault_tick_interval_ms`, `fetch_concurrency`, `backfill_sig_limit`, `reconnect_backoff_max_ms` | public RPC / `confirmed` / `1000` / `5000` / `32` / `100` / `5000` |
| `trading`   | `start_balance`, `fee_bps`, `notional_usd`, `max_open_positions`, `max_exposure_usd`, `mint_cooldown_secs`, `max_daily_loss_usd`, `hot_wallet_max_sol`, `quote_usd`*, `liquidity_usd`                  | `100` / `0` / `10` / `3` / `0` (off) / `3600` / `0` (off) / `0` (off) / unset / `0`        |
| `exits`     | `trail_percent`, `runner_trail_percent`                                                                                                                                                              | `5` / `20`                                                         |
| `gate`      | `gate_window_secs`, `gate_min_buyers`, `gate_poll_ms`, `dev_min_age_secs`, `dev_max_prior_launches`, `dev_screen_sample_txs`, `dev_sig_limit`                                                        | `10` / `5` / `2000` / `3600` / `1` / `20` / `100`                  |
| `risk`      | `strategy_mode`, `runner_moonbag_percent`, `inference_endpoint`, `inference_model`, `inference_timeout_ms`, `inference_headers`                                                                      | `adaptive` / `20` / local / `llama3.2:1b` / `300` / unset          |
| `execution` | `slippage_bps`, `slippage_vol_window_secs`, `slippage_vol_extra_cap_bps`, `priority_fee_micro_lamports`, `compute_unit_limit`, `jito_tip_lamports`, `jito_endpoint`, `sim_strict`                    | `200` / `0` / `0` / `0` / `0` / `100000` / global / `false`        |
| `alerts`    | `discord_alerts`, `discord_channel`, `alert_diverged_warn_n`, `alert_risk_fallback_warn_pct`, `alert_giveup_burst_n`, `alert_giveup_window_secs`                                                     | `true` / host default / `3` / `50` / `3` / `3600`                  |

- `quote_usd` is test-only.
- `yellowstone_endpoint`: non-empty selects Yellowstone gRPC ingestion instead of WS (x-token via `YELLOWSTONE_TOKEN` env, never the file).

- `fetch_concurrency` bounds parallel `getTransaction` fetches (lower on metered RPC; `0` refuses at boot); `backfill_sig_limit` caps sigs per reconnect backfill (`0` disables it); `reconnect_backoff_max_ms` caps reconnect backoff (raise on flaky sockets).
- `strategy_mode`: `adaptive` runs the profile verbatim; `scalp` clamps TP to +25…+35%; `runner` sells 80%, trails the 20% moonbag toward 5x–10x.
- Risk limits (all optional, `0` = off; exits are never blocked by any of them): `notional_usd` caps one ticket, `max_open_positions` caps concurrent positions (paper + live-held mints), `max_exposure_usd` caps total open exposure in USD (paper cost basis + one ticket per unpaired live bag — a bag carries no stored USD basis), `max_daily_loss_usd` halts entries past the day's realized loss. The day-loss tally persists in `~/.pump-sentry/day-state.json`, so a restart cannot reset either the tally or the cap it enforces; live-held custody is restored from the WAL and counts against heat/exposure after a restart too.
- `hot_wallet_max_sol`: entries refuse (buy preflight) while the wallet holds more SOL than this. The operator bounds what an un-swept wallet or a leaked key can lose; `sell`/exits never consult it. Sweeping is the fix, not raising the cap.
- `inference_headers`: optional extra headers, one or more comma-separated as `"Name: value, Other: value"` (e.g. OpenCode Go requires `x-opencode-session` for routing/cache); empty (default) sends none. Values may contain `:` (first-colon split) but not `,` (entry separator); repeats resolve last-wins. Malformed and transport-owned entries are skipped with a warning naming the header only (never the value). `authorization`/`user-agent`/framing headers are refused — auth stays in `INFERENCE_API_KEY` env, the user agent is owned.

Settings that live **outside** the file (env-only, never in `config.toml`):

| Variable(s)                                                                           | Purpose                                                        |
|---------------------------------------------------------------------------------------|----------------------------------------------------------------|
| `IS_LIVE`                                                                             | safety-critical, exact `true` per invocation, same as `--live` |
| `SOLANA_PRIVATE_KEY`                                                                  | moves money, live only                                         |
| `SOLANA_PRIVATE_KEY_FILE`                                                             | preferred: 0600 key file, avoids exporting the key             |
| `INFERENCE_API_KEY`                                                                   | bearer token, scrubbed after boot                              |
| `INFERENCE_API_KEY_FILE`                                                              | preferred: 0600 bearer file, avoids exporting the token        |
| `YELLOWSTONE_TOKEN`                                                                   | geyser x-token (auth + billing)                                |
| `YELLOWSTONE_TOKEN_FILE`                                                              | preferred: 0600 token file, avoids exporting the token         |
| `CONFIG_FILE` / `PID_FILE` / `LOG_FILE`                                               | path overrides                                                 |
| `BOT_DAEMON` / `BOT_INVOCATION` / `BOT_FIRST_RUN`                                     | daemon markers                                                 |
| `HERMES_BIN`                                                                          | alert binary override                                          |
| `JITO_ACCEPTANCE`, `MAINNET_LAYOUT`, `RAYDIUM_ACCEPTANCE` (`*_ACCEPTANCE` test gates) | test gates, ignored by default                                 |

## Going live

Defaults are paper-safe. Change only these for live — each value below
traces to observed behavior, not taste. Run paper 1–2 weeks first and
read the heartbeat (diverged count, fallback rate, execution gap,
watchlist→open ratio) before funding anything.

```toml
[network]
mainnet_rpc_url = "https://..."      # dedicated (Helius): public RPC drops
                                     # candidates and truncates quorum reads (observed)
mainnet_wss_rpc_url = ""             # unused while Yellowstone is set
yellowstone_endpoint = "https://solana-yellowstone-grpc.publicnode.com:443"
                                     # x-token via YELLOWSTONE_TOKEN env, never here
commitment = "confirmed"             # processed is refused live

[trading]
notional_usd = 20.0                  # $10 tickets drown in ~6% fixed bleed;
                                     # $20 halves the fixed share
max_open_positions = 2               # fewer concurrent mouths (~$40 exposure + costs)
max_daily_loss_usd = 5.0             # breaker is a live boot requirement;
                                     # ~12% of exposure; raise only from burn-in data

[gate]
gate_window_secs = 15                # default 10 loses to RugCheck indexing
                                     # latency (observed 400 streaks); +50% time, modest
# buyers/dev/liquidity: keep defaults (5 buyers, 3600s age, max 1 prior —
# the rejections in live logs prove these filters earn their keep)

[risk]
strategy_mode = "adaptive"           # verbatim profile; scalp/runner reshape edge
inference_timeout_ms = 800           # ONLY for a remote gateway;
                                     # localhost Ollama keeps 300 (slower entries cost price)
# inference_endpoint/model point at the gateway's /v1/chat/completions
# inference_headers = "x-opencode-session: <session-id>"  # ONLY for providers
                                     # that require extra headers (e.g. OpenCode Go
                                     # routing); empty/absent sends none;
                                     # more than one: comma-separated
                                     # ("Name: value, Other: value")

[execution]
slippage_bps = 200                   # keep; widen only from execution-gap data
jito_tip_lamports = 100000           # keep; priority/adaptive stay 0 until congestion data
sim_strict = true                    # boot-enforced live; non-negotiable

[alerts]
# keep all defaults ON (3/50/3): silencing them is accepting blindness
```

### A slightly aggressive variant (still safe)

Same base, tuned for more attempts and faster exits — not looser
filters. Principle: aggression lives in **sizing, speed, and landing**;
safety lives in **unchanged filters, breaker, and fail-closeds**.
Change only these lines, validate on paper first, promote only when the
heartbeat stays clean.

```toml
[network]
tick_interval_ms = 500           # faster curve polls; dedicated RPC only
fetch_concurrency = 64           # fewer throttled drops in bursts

[trading]
notional_usd = 25.0              # exposure $75 at 3 open — size the breaker to match
max_open_positions = 3           # more concurrent mouths ($75 exposure + costs)
max_exposure_usd = 75.0          # cap total exposure at the same $75 (optional; 0 = off)
max_daily_loss_usd = 8.0         # ~11% of exposure, same ratio as base
hot_wallet_max_sol = 1.0         # optional: buys refuse above this wallet balance
mint_cooldown_secs = 1800        # faster re-entry; still no instant revenge trades

[gate]
gate_window_secs = 20            # wider net; entries skew slightly older
gate_min_buyers = 4              # more opens, still a real quorum (never below 3
                                 # without burn-in data; floor is 1)
gate_poll_ms = 1000              # faster quorum confirm; more RPC load

[exits]
trail_percent = 3.0              # locks profit sooner; more exits on noise

[risk]
strategy_mode = "runner"         # main TP exits, 30% moonbag rides toward 5x–10x
                                 # with a forced trail (never trailless)
runner_moonbag_percent = 30.0    # 0–100; >= 100 coerces to 0 (full exit at TP)

[execution]
slippage_bps = 300               # tolerates 3% adverse move per side to land fills;
                                 # sells still fail closed, boot refuses ≥100%
jito_tip_lamports = 150000       # better landing; funding scales as tip × positions × 2
```

Do NOT touch for aggression: `dev_*` screens, RugCheck audit, quorum
floor, non-6-decimal and Token-2022 fail-closeds, `max_daily_loss_usd > 0`,
`sim_strict = true`, all `alert_*` burn-in defaults, `commitment =
"confirmed"`, `QUOTE_USD` unset.

Live env (all env-only): `SOLANA_PRIVATE_KEY` on a **dedicated** wallet,
`YELLOWSTONE_TOKEN`, optional `INFERENCE_API_KEY`. Fund native SOL per
`ops/RUNBOOK.md` §2 (trade + fees + tips + 0.05 reserve), verify Discord
cards actually arrive (Hermes timeouts observed), and keep `notional_usd`
small until the execution gap line says otherwise. Prefer
`SOLANA_PRIVATE_KEY_FILE` (the bot **refuses** a key file that is
group/world readable) and set `hot_wallet_max_sol` so the wallet never
sits above what you are willing to lose.

## Safety

**Golden rules (everyone):**

- Paper-only without `--live`/`IS_LIVE=true`. Live needs a dedicated wallet, small `notional_usd`, `max_daily_loss_usd>0`, `sim_strict=true`.
- Exits never block: TP/SL/trailing/timeout + manual `sell`; the day-loss breaker halts entries only.
- `pump-sentry pause` is the emergency kill switch: no new entry opens (the marker is polled every gate round, no restart needed), while exits, runner/moonbag scale-outs, and `sell <mint>` keep managing open positions. `pump-sentry resume` re-arms entries. The marker survives restarts.

**What it refuses (fail-closed, no funds move):**

- Audit rejects, fresh/serial insiders, non-6-decimal mints, no-quorum timeouts, dev dumps, bundled supply monopolies, unconfirmed fills (re-polled, never assumed).
- Post-migration exits route through the validated pool leg (Token-2022 vaults refuse fail-closed).

**Paper is cost-honest, not live-identical:** the ledger nets venue fees plus dispatch bleed (priority fee included, calm-weather floor), and the heartbeat reports the running paper-vs-live execution gap. Size live expectations from that gap, never from the paper equity curve alone.

## Layout

```
src/               # cli (supervisor) / settings / pipeline (orchestrator) / live (sizing+dispatch)
                   # gate (watchlist+opens) / live_mirror (live dispatch) / follow (migration watch) / wal (custody log)
                   # day_state (restart-persistent day-loss tally) / money (validated Bps/Percent units)
crates/listener/   # WS ingestion + parser + screening + quorum gate
crates/executor/   # live swaps via Jito (only with --live)
crates/curve|auditor|risk-engine|simulator|ids|alert|migration/
```

Each crate has its own README. Tests: `cargo test --workspace` (live probes ignored) · `cargo clippy --all-targets` (zero warnings).

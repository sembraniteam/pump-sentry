# pump-sentry

Paper-trading sniper for new Pump.fun launches.

It watches every new launch, throws away scams and weak setups through
several filters, opens **virtual (paper) positions**, and exits them
automatically. No real money moves unless you explicitly enable live
mode (see [Going live](#going-live)).

Pipeline: `logsSubscribe` → RugCheck audit → dev screen → buyer quorum → bundle forensics → TP/SL profile (LLM ≤300ms, else heuristic) → paper position → auto-exit → migration hunt → pool tracking.

## Quickstart (paper mode, no money involved)

1. **Install.** You need a mainnet RPC (dedicated recommended); Rust 1.99 matters for source builds — it is pinned by `rust-toolchain.toml`, and rustup installs the 1.99 channel automatically on the first `cargo` invocation.

   ```sh
   curl -fsSL https://raw.githubusercontent.com/sembraniteam/pump-sentry/main/install.sh | bash
   # pin a release: VERSION=vX.Y.Z bash install.sh · build from source: bash install.sh --from-source · uninstall.sh [--purge]
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
| `pump-sentry replay [file]` | re-derive recorded decisions offline from `journal.jsonl`; exit 1 on any mismatch/unverifiable step |
| `pump-sentry research [file]` | offline loss-pattern analysis over the journal: loss clusters plus ranked filter proposals with avoided-vs-foregone math and halves; read-only by construction |
| `pump-sentry discord` | run the Discord sidecar (separate process): Tier 1 read-only slash commands (`/status`, `/heartbeat`, `/positions`, `/pnl`, `/watchlist`, `/config`), Tier 3 read-only analysis (`/research`, `/risk`, `/token`) plus Tier 2 allowlisted control (`/sell`, `/pause` direct, `/resume` with button confirm); writes only the existing control files, never trades itself; needs `DISCORD_BOT_TOKEN` and a `discord.toml` (`guild_id` + `operator_ids`), takes no lock |

State lives in `~/.pump-sentry/` (`bot.log`, `bot.pid`, `bot.lock`, sell requests, WAL, day-state, kill-switch marker, snapshots); `LOG_FILE`/`PID_FILE` override it, and the exclusive `bot.lock` (flock) refuses a second process on the same state dir before it reads config or custody. Long runs need `ops/logrotate/pump-sentry`.

Live funding: SOL-pair v3 buys pay **native SOL** — fund native for trade + fees + tips + 0.05 reserve. A **WSOL** float is only needed for token-paired quotes.

## Decision journal & offline replay

Every decision the bot makes is appended to `journal.jsonl` (next to the log):
boot settings, each price tick that drove a check, paper opens/exits (exact
booked numbers and trigger reasons), live dispatch attempts with verdicts and
real costs, settled execution gaps, and equity heartbeats. It is
**analytics-only and fail-soft**: journal write failures log a warning and
degrade recording, but trading never stops because of it (unlike the custody
WAL).

`pump-sentry replay` re-derives the recorded decisions from recorded inputs
only — no lookahead — and prints two strictly separated metric buckets:

- **paper** (simulator: model fills, booked fees): trades, win rate, net PnL,
  expectancy, avg cost/trade, drawdown;
- **live** (chain outcomes): buy attempts → fill rate, sell give-ups →
  failed-exit rate, settled execution gaps, committed costs, and a
  live-net estimate.

Paper numbers describe the simulator only; paper win rate is NOT evidence of
live profitability — judge live by the live bucket alone. An exit whose input
tick line is missing reports as *unverifiable* and fails the exit code (fail
closed, never assumed).

Replay also bins expectancy per entry attribute, each with chronological
halves and a thin-bin floor (`n < 5` is marked, never read as evidence):

- **decision source** — model vs heuristic-fallback trades (a fallback that
  protects volume at the cost of edge shows here, not hidden by the rate);
- **risk model** — expectancy per named engine plus `not recorded` (no
  engine or pre-upgrade lines): the journal spans boots, so this is the
  cross-run per-model performance read;
- **exit regime** — what each trade actually ran (LLM pick or the
  `force_exit_regime` baseline);
- plus microstructure joins — buy pressure, round concentration, insider
  flow, tape velocity.

`pump-sentry research` extends this offline (read-only): it clusters losses
by regime / pressure / concentration / risk model and ranks candidate
exclusion rules with the loss they would have avoided against the wins they
would have foregone. A proposal "wins" only when the halves agree — and
promoting one always remains operator work: config edit, paper backtest,
live confirm, explicit approval. The command cannot apply anything.

## Resilience

Reads are redundant, dispatch is not. Every RPC read (executor, tickers,
screener, backfill) shares one client wrapping a failover pool: a failed
or hung attempt retries on the next healthy endpoint (6s light / 25s
block-weight budgets, under the callers' 10s outers so a hung primary
actually fails over), latency is tracked per endpoint, and a 10s sampler
doubles as health check plus staleness detector (a primary lagging the
freshest peer goes stale; disagreeing views flag inconsistent). One
logical read hits exactly one endpoint at a time — never an all-endpoints
broadcast — and dispatch never rides the pool: one decision, one Jito
bundle, one endpoint.

When a critical source is unhealthy the bot stops deciding instead of
deciding on stale data. A supervisor pauses entries through the same
choke point as the kill switch (`open_tracked`; exits and `sell <mint>`
keep running, recovery clears it automatically). Triggers, all
evidence-based: pool outage (sampler-confirmed, never a single blip),
lost SOL/USD price (a boot that never priced gets a 5-minute grace;
losing a had price pauses at once), banned ingestion, or a stream silent
for 30 minutes (covers WS mode too — launches are seconds apart, so
silence is a dead stream, never a quiet chain). Live evidence trips the
same pause: buy fill rate below `min_fill_rate_pct` over trailing settled
outcomes, paper-vs-live gap averages worse than `max_avg_exit_gap_usd` /
`max_avg_entry_gap_usd` (both off until calibrated in USD per ticket),
or a ledger balance short of the claims. Thin windows never trip;
recovery is always a passing re-check, never elapsed time.

Stream gaps are counted, not hidden: geyser backfill sheds windows past its slot cap or its
30s reconnect budget (`slot_gaps_shed`, failed/pruned blocks included); WS backfill pages up
to 3 signature pages and counts capped remainders (`backfill_pages_capped`).

Custody is re-verified, not just replayed: the live WAL stamps a schema
line per boot (provenance only — old binaries skip it) and compaction
keeps every stamp, so the file names all its writers; `day-state.json`
carries a schema version (a newer writer's file degrades to a fresh day,
loudly, never adopting guessed numbers). On boot, every recovered bag
hunts its migration immediately (the curve may already be graduated and
stop ticking) and registers the pool leg; unresolved bags are surfaced
as having no pool exit feed and retried on the supervision recheck.
Every 30 minutes live mode
re-checks WAL claims per mint against the chain ATA balance — read-only;
mismatches page the operator, nothing auto-corrects. A short ledger
(balance below claims) additionally latches the entry pause until a later
recheck passes. The heartbeat card
carries the health view: RPC `n/m ok · failovers`, data state, oldest
open age, stream gaps shed, and windowed GiveUps; a paper-ledger
overdraft (dispatch costs past cash) warns on the same beat, and a
position overdue past its own timeout warns log-only (runner lifts
respected).

## Uninstall

```sh
pump-sentry stop            # stop the daemon first (the script won't)
curl -fsSL https://raw.githubusercontent.com/sembraniteam/pump-sentry/main/uninstall.sh | bash
# curl -fsSL .../uninstall.sh | bash -s -- --purge   # also remove ~/.pump-sentry (config, logs, WAL, snapshots)
```

The script removes the binary from `/usr/local/bin`, `~/.local/bin`, and `~/.pump-sentry/bin` (`INSTALL_DIR` overrides the search path). It never touches your shell profile: unset `SOLANA_PRIVATE_KEY` / `INFERENCE_API_KEY` / RPC URL vars there yourself, and delete the repo clone if you built from source. Reinstalling without `--purge` resumes from the kept state.

## Config

`~/.pump-sentry/config.toml`, grouped by `[section]` — one key per line.
This file is the **only source for tunables** — env vars are not read for them.
Secrets (`SOLANA_PRIVATE_KEY`, `INFERENCE_API_KEY`) stay in env, never in the file.

| Section     | Keys                                                                                                                                                                                                                                                                                                                                       | Defaults                                                                                                                                               |
|-------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------|
| `network`   | `mainnet_rpc_url`, `mainnet_rpc_urls_fallback`, `mainnet_wss_rpc_url`, `yellowstone_endpoint`*, `commitment`, `tick_interval_ms`, `vault_tick_interval_ms`, `fetch_concurrency`, `backfill_sig_limit`, `reconnect_backoff_max_ms`                                                                                                          | public RPC / `""` (primary-only) / `confirmed` / `1000` / `5000` / `32` / `100` / `5000`                                                               |
| `trading`   | `start_balance`, `fee_bps`, `notional_usd`, `max_open_positions`, `max_exposure_usd`, `mint_cooldown_secs`, `max_daily_loss_usd`, `hot_wallet_max_sol`, `quote_usd`*, `liquidity_usd`, `max_expected_entry_gap_usd`, `max_top_holder_pct`, `max_entry_flip_loss_pct`, `min_fill_rate_pct`, `max_avg_exit_gap_usd`, `max_avg_entry_gap_usd` | `100` / `0` / `10` / `3` / `0` (off) / `3600` / `0` (off) / `0` (off) / unset / `0` / `0` (off) / `0` (off) / `0` (off) / `50` / `0` (off) / `0` (off) |
| `exits`     | `trail_percent`, `runner_trail_percent`                                                                                                                                                                                                                                                                                                    | `5` / `20`                                                                                                                                             |
| `gate`      | `gate_window_secs`, `gate_min_buyers`, `gate_poll_ms`, `dev_min_age_secs`, `dev_max_prior_launches`, `dev_screen_sample_txs`, `dev_sig_limit`                                                                                                                                                                                              | `10` / `5` / `2000` / `3600` / `1` / `20` / `100`                                                                                                      |
| `risk`      | `strategy_mode`, `runner_moonbag_percent`, `inference_endpoint`, `inference_model`, `inference_timeout_ms`, `inference_fallback`, `inference_defer_max_n`, `inference_headers`, `force_exit_regime`                                                                                                                                        | `adaptive` / `20` / local / `llama3.2:1b` / `300` / `heuristic` / `1` / unset / `""`                                                                   |
| `execution` | `slippage_bps`, `slippage_vol_window_secs`, `slippage_vol_extra_cap_bps`, `priority_fee_micro_lamports`, `compute_unit_limit`, `jito_tip_lamports`, `jito_endpoint`, `sim_strict`                                                                                                                                                          | `200` / `0` / `0` / `0` / `0` / `100000` / global / `false`                                                                                            |
| `alerts`    | `discord_alerts`, `discord_channel`, `alert_diverged_warn_n`, `alert_risk_fallback_warn_pct`, `alert_giveup_burst_n`, `alert_giveup_window_secs`                                                                                                                                                                                           | `true` / unset (sends disabled) / `3` / `50` / `3` / `3600`                                                                                            |

- `quote_usd` is test-only.
- `dev_screen_sample_txs` is a floor for the signature budget (`dev_sig_limit`, clamped to 1000); it does not truncate the dev-history count — the screen walks up to 3 pages and fails closed when that budget cuts the history short.
- `fee_bps` (extra commission per side, on top of the venue fee already inside fills), `trail_percent` and `runner_trail_percent` are validated through typed units: negative/non-finite values warn, `fee_bps >= 100%` fails closed in paper settlement, and trail values must be in `(0, 100]`. In paper mode these are advisory warnings, never a boot refusal; live mode refuses an invalid `fee_bps` outright (an out-of-range commission can neither widen real stop-losses nor corrupt bookkeeping), and the gate's fee-aware stop widening only consumes the typed value — an invalid raw value contributes no widening.
- `yellowstone_endpoint`: non-empty selects Yellowstone gRPC ingestion instead of WS (x-token via `YELLOWSTONE_TOKEN` env, never the file).

- `fetch_concurrency` bounds parallel `getTransaction` fetches (lower on metered RPC; `0` refuses at boot); `backfill_sig_limit` caps signatures per backfill page (`0` disables backfill) — the WS reconnect backfill pages the signature list up to 3 pages seeking the last seen signature, past the cap with unseen history beyond it the remainder counts as lost (`backfill_pages_capped`, warn) instead of paging forever; `reconnect_backoff_max_ms` caps reconnect backoff (raise on flaky sockets).
- `mainnet_rpc_urls_fallback` (config-file key; the env-style spelling `MAINNET_RPC_URLS_FALLBACK` is the parser's flat key name, not a process-env source — nothing reads the environment for tunables), comma-separated **string** (a TOML array is skipped with a boot warning): read-path failover endpoints behind the one shared RPC client — primary first, next only after an attempt fails (failover, never broadcast). Reads only: order dispatch stays single-path Jito and never fans out. Empty (default) keeps today's primary-only shape. All endpoint URLs (RPC, WS ingestion, fallbacks, Jito, Yellowstone) are trust-checked at boot: a malformed/wrong-scheme URL, a remote plaintext endpoint (`http://`/`ws://`; localhost is exempt), a fallback that identifies a different cluster than the primary RPC, or a WS URL naming another cluster refuse a live boot (paper mode warns and continues). The cross-cluster fallback refusal can be explicitly overridden with `ALLOW_FALLBACK_CLUSTER_MISMATCH=1` (logged; WS never). Live mode additionally refuses the shipped public RPC default (`api.mainnet-beta.solana.com`, any spelling) outright unless the operator sets `ALLOW_PUBLIC_RPC_LIVE=1` (logged) — the public endpoint is documented here as unfit for live reads (drops candidates, truncates quorum, rate limits).
- `strategy_mode`: `adaptive` runs the profile verbatim; `scalp` clamps TP to +25…+35%; `runner` sells 80%, trails the 20% moonbag toward 5x–10x.
- `force_exit_regime` (`""` = the model selects): pins the exit profile to a closed regime (`momentum` / `conservative` / `liquidity_sensitive`) for evaluation and as the LLM kill path. With selection on, the model only picks from that closed set and a deterministic snap replaces free-form numbers; operator shaping (`scalp`/`runner`) still applies after.
- Risk limits (all optional, `0` = off; exits are never blocked by any of them): `notional_usd` caps one ticket, `max_open_positions` caps concurrent positions (paper + live-held mints), `max_exposure_usd` caps total open exposure in USD (paper cost basis + one ticket per unpaired live bag — a bag carries no stored USD basis), `max_daily_loss_usd` halts entries past the day's realized loss, and `max_expected_entry_gap_usd` / `max_top_holder_pct` / `max_entry_flip_loss_pct` refuse tickets with proven entry bleed / over-concentrated supply / a measured open+sell-back round-trip cost above the cap (entry impact + exit impact + venue fees priced at the confirm book by `FillModel::flip_preview`, the same preview the risk model sees; unknown/unpriceable always allows). The day-loss tally persists in `~/.pump-sentry/day-state.json`, so a restart cannot reset either the tally or the cap it enforces; live-held custody is restored from the WAL and counts against heat/exposure after a restart too. Live exits with no paper leg (WAL-recovered bags) book realized PnL from the entry cost persisted with the bag; a legacy line without that cost is counted and warned as unaccounted, never silently treated as zero.
- `hot_wallet_max_sol`: entries refuse (buy preflight) while the wallet holds more SOL than this. The operator bounds what an un-swept wallet or a leaked key can lose; `sell`/exits never consult it. Sweeping is the fix, not raising the cap.
- `inference_headers`: optional extra headers, one or more comma-separated as `"Name: value, Other: value"` (e.g. OpenCode Go requires `x-opencode-session` for routing/cache); empty (default) sends none. Values may contain `:` (first-colon split) but not `,` (entry separator); repeats resolve last-wins. Malformed and transport-owned entries are skipped with a warning naming the header only (never the value). `authorization`/`user-agent`/framing headers are refused — auth stays in `INFERENCE_API_KEY` env, the user agent is owned.
- `inference_fallback`: what happens on a model failure at confirm — `heuristic` (default: validated bands open), `pass` (skip the entry), `defer` (re-watch the signal and retry the model on a later confirm; buyers re-gather, and the defer count — not the deadline — is the hard stop, since each defer starts a fresh watch window). Engine-absent mode is deliberate, not a failure: it never defers. `inference_defer_max_n` caps defers per signal (`0` = off; exhausted defers land on the heuristic). Per-model performance is read offline — the journal records the engine name on every open (`pump-sentry replay` bins a `risk model` expectancy table across boots; `pump-sentry research` clusters losses by model) — and the heartbeat carries live latency/fallback telemetry per booted model.

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
| `ALLOW_PUBLIC_RPC_LIVE`                                                               | explicit live override for the public RPC default              |
| `ALLOW_FALLBACK_CLUSTER_MISMATCH`                                                     | explicit override for cross-cluster read fallbacks             |
| `DISCORD_BOT_TOKEN`                                                                 | Discord bot token, scrubbed after boot                         |
| `DISCORD_BOT_TOKEN_FILE`                                                            | preferred: 0600 token file, avoids exporting the token        |
| `JITO_ACCEPTANCE`, `MAINNET_LAYOUT`, `RAYDIUM_ACCEPTANCE`                             | live-probe gates, ignored by default                           |
| `PUMP_ACCEPTANCE_POOL`, `PUMP_ACCEPTANCE_HOLDER`, `MAINNET_RPC_URL`                   | live probes only (executor tests): creatorless-fee target, RPC override; the bot never reads these |

## Going live

Defaults are paper-safe. Change only these for live — each value below
traces to observed behavior, not taste. Run paper 1–2 weeks first and
read the heartbeat (diverged count, fallback rate, execution gap,
watchlist→open ratio) before funding anything.

```toml
[network]
mainnet_rpc_url = "https://..."      # dedicated (Helius): public RPC drops
                                     # candidates and truncates quorum reads (observed)
mainnet_rpc_urls_fallback = ""       # optional second RPC for read failover
                                     # (comma-separated string, never a TOML
                                     # array; reads only; dispatch stays single-path)
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
min_fill_rate_pct = 50.0             # keep: pauses entries while fills collapse
max_top_holder_pct = 0.0             # keep off until holder data says otherwise
max_entry_flip_loss_pct = 0.0        # keep off until burn-in: refuse opens whose
                                     # measured open+sell-back cost exceeds this %
max_avg_exit_gap_usd = 0.0           # keep off: calibrate from the heartbeat
max_avg_entry_gap_usd = 0.0          # gap averages before arming either trip

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
`YELLOWSTONE_TOKEN`, optional `INFERENCE_API_KEY`. Fund native SOL for
trade + fees + tips + the 0.05 reserve, verify Discord
cards actually arrive (verify on paper first), and keep `notional_usd`
small until the execution gap line says otherwise. Prefer
`SOLANA_PRIVATE_KEY_FILE` (the bot **refuses** a key file that is
group/world readable) and set `hot_wallet_max_sol` so the wallet never
sits above what you are willing to lose.

Supply-chain gate (release CI): `cargo audit --deny warnings` runs with
**no ignore list**. The Discord sidecar used serenity's rustls backend, pinning `rustls 0.22` +
`rustls-webpki 0.102` (7 RustSec advisories) alongside `ctutils 0.4.3`
(1 advisory) in the Solana tree; none of them remain in
`Cargo.lock` — the sidecar now uses the serenity `native_tls_backend`
(system TLS; on Linux this links OpenSSL, so `libssl` becomes a runtime
dependency), and `ctutils` is bumped past its advisory. Sells keep a
fee-leg-only solvency floor (exits never block on the 0.05 SOL buy
reserve).

## Safety

**Golden rules (everyone):**

- Paper-only without `--live`/`IS_LIVE=true`. Live needs a dedicated wallet, small `notional_usd`, `max_daily_loss_usd>0`, `sim_strict=true`.
- Exits never block: TP/SL/trailing/timeout + manual `sell`; exit ticks and manual sells draw from a reserved dispatch lane so a launch flood cannot starve them, and the day-loss breaker halts entries only.
- `pump-sentry pause` is the emergency kill switch: no new entry opens (the marker is polled every gate round, no restart needed), while exits, runner/moonbag scale-outs, and `sell <mint>` keep managing open positions. `pump-sentry resume` re-arms entries. The marker survives restarts.

**What it refuses (fail-closed, no funds move):**

- Audit rejects, fresh/serial insiders, non-6-decimal mints, no-quorum timeouts, dev dumps, bundled supply monopolies, unconfirmed fills (re-polled, never assumed).
- Ticket shapes with proven entry bleed, over-concentrated top holders (both opt-in caps, thin evidence always allows).
- Post-migration exits route through the validated pool leg (Token-2022 vaults refuse fail-closed).

**Paper is cost-honest, not live-identical:** the ledger nets venue fees plus dispatch bleed (priority fee included, calm-weather floor), and the heartbeat reports the running paper-vs-live execution gap. Size live expectations from that gap, never from the paper equity curve alone.

## Layout

```
src/               # cli (supervisor) / settings / pipeline (orchestrator) / live (sizing+dispatch)
                   # gate (watchlist+opens) / live_mirror (live dispatch) / follow (migration watch) / wal (custody log)
                   # journal (decision record) / replay (offline verifier)
                   # day_state (restart-persistent day-loss tally) / money (validated Bps/Percent units)
crates/listener/   # WS ingestion + parser + screening + quorum gate
crates/executor/   # live swaps via Jito (only with --live)
crates/curve|auditor|risk-engine|simulator|ids|alert|migration/
```

Each crate has its own README. Tests: `cargo test --workspace` (live probes ignored) · `cargo fmt --all -- --check` · `cargo clippy --workspace --all-targets` (zero warnings).

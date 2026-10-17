# Live operations runbook — pump-sentry

Paper mode needs nothing here. Live mode (`--live` / `IS_LIVE=true`)
moves real SOL: do every item in order, skip nothing.

## 1. Pre-flight checklist (boot refuses without these)

Tunables live in `~/.pump-sentry/config.toml` by category (auto-created; the file is the only source, secrets stay in env).

- [ ] `max_daily_loss_usd>0` — day-loss breaker (exits always work).
- [ ] `sim_strict=true` — unproven sims refuse instead of warn-and-proceed.
- [ ] `jito_tip_lamports>0` — tipless bundles may never land.
- [ ] `commitment=confirmed|finalized` — `processed` is refused (reorgable prints).
- [ ] `quote_usd` unset — static fallback is paper-only and refused live.
- [ ] `mainnet_v3_17_simulates` GREEN on this exact binary
  (`MAINNET_LAYOUT=1 cargo test -p executor --test mainnet_layout_acceptance -- --ignored`).
- [ ] `mainnet_bundle_acceptance` GREEN (Jito base64 wire proof, $0 probe:
  `JITO_ACCEPTANCE=1 cargo test -p executor --test jito_acceptance -- --ignored`).
- [ ] `raydium_authority_and_bundle_simulate` GREEN (post-migration exit proof:
  `RAYDIUM_ACCEPTANCE=1 cargo test -p executor --test raydium_authority_acceptance -- --ignored`).
  All three are simulation-only ($0 by construction); rerun on router-dominated
  samples — resampling is the procedure, not a failure.
- [ ] RPC and Jito serve the same cluster (boot checks URL substrings;
  devnet RPC + mainnet Jito can never land).

## 2. Funding math (native SOL first)

SOL-pair v3 buys transfer **native SOL**. Fund native for everything;
no WSOL float needed for SOL pairs:

```
required_native = max_open_positions × (notional_usd/SOL_USD × (1+slippage_bps/10000) + fees)
                + max_open_positions × 2 × (jito_tip_lamports + priority_per_dispatch + 5_000 base)  // buy + sell legs
                + max_open_positions × ~2_039_280 lamports (one ATA rent per new mint; assumes all mints new)
                + 0.05 SOL reserve (hard floor, never spent)
```

where priority_per_dispatch = compute_unit_limit × priority_fee_micro_lamports / 1e6.
Boot enforces the dispatch part via the scaled funding report (notional
legs are checked per trade by solvency).

A WSOL float funds **token-paired quotes only**. Boot fails closed
below the scaled requirement and warns loudly on zero WSOL.

## 3. Start / stop / inspect

```sh
pump-sentry start --live   # daemon; state in ~/.pump-sentry/
pump-sentry status          # running? (pid, mode, log)
pump-sentry logs -n 50      # tail; --follow to follow
pump-sentry sell <mint>     # manual full exit now (only way parked bags leave)
pump-sentry stop            # SIGTERM, 30s drain, snapshot, SIGKILL past 45s
```

`sell` follows daemon mode: it exits the live bag in live mode, and
closes the paper position in paper mode (cards always state the mode).

`sell <mint>` files drain every 15 gate rounds; the request file is
deleted only after the live leg settles (sold / manual-parked /
nothing held). A kept file is normal while the live leg still holds.

## 4. Reading state (shutdown snapshot + WAL)

- `~/.pump-sentry/shutdown-<ts>.json`: `paper_open`, `live_tracked`
  (base mints — suffixed `#2`/`#late` duplicates collapse here),
  `watchlist_pending`, `clean`. `clean:false` means fills are UNKNOWN —
  look every tracked mint up on the explorer before restarting.
- `~/.pump-sentry/live-wal.jsonl`: append-only custody log, fsync'd
  per line. **Never delete it** unless every tracked/manual bag is
  verified exited on-chain; boot replays it into pending/positions/
  manual. Corrupt lines are skipped loudly (`skipped` count).

## 5. Parked bags (`MINT`, `MINT#2-…`, `MINT#late-…`)

Give-ups and unknown fills park here and stop automated probing —
this is custody, not a bug. Exit each via `sell <mint>`. Second fills
park suffixed without clobbering the first; review every suffixed bag
on the explorer (bundle UUID in the alert card).

## 6. Logs (no restart needed)

Install `ops/logrotate/pump-sentry` (rename + SIGHUP reopen —
lossless). **Never use `copytruncate`** (drops lines mid-rotation).
The supervisor also rotates ≥50 MB logs at start time as a backstop.

## 7. When something looks wrong

| Symptom                       | Likely cause                                                         | Action                                                            |
|-------------------------------|----------------------------------------------------------------------|-------------------------------------------------------------------|
| No entries for hours          | Gate quorums unmet / day-loss tripped / public RPC throttled (drops) | `logs`, heartbeat card, use dedicated RPC                         |
| `diverged` alerts             | Live buy skipped/failed, paper uncovered                             | Review, fund, or accept paper-only                                |
| Exits stalled, SOL/USD stale  | Price feed down (DexScreener; Jupiter retired)                           | By design (no static fallback); exits resume on recovery          |
| `MANUAL REVIEW REQUIRED`      | Unknown fill / retries exhausted                                     | Explorer first, then `sell <mint>`                                |
| `INGESTION BANNED` card       | Geyser 429s for ~30 min straight (quota/exhaustion, not burst)       | **Stop the bot.** Check token/quota/dashboard; wait out the cycle or upgrade tier. To keep running blind-free: clear `yellowstone_endpoint` (WS mode) and restart. Retries hourly on its own; backfill covers only the newest ~25s after recovery |
| Negative execution-gap avg    | Live trails paper (congestion tips, landing latency, stale exits)     | Widen slippage, lower notional, or accept the haircut explicitly  |
| Duplicate cards after restart | Crash between dispatch and WAL                                       | WAL replay is conservative; verify on-chain, exit dups via `sell` |

## 8. Burn-in observables (first live week)

Thresholds below are **on by default** (log-only, never Discord
cards). Tune from what you observe; the heartbeat also prints the
execution-gap line (`n/avg/total` USD, paper model vs live settle +
tips) — a persistently negative avg is the live haircut made visible.

| `[alerts]` key (default)                              | Fires when                                        | Meaning / action                                                                          |
|-------------------------------------------------------|---------------------------------------------------|-------------------------------------------------------------------------------------------|
| `alert_diverged_warn_n` (`3`)                         | Heartbeat diverged count ≥ N                      | Paper without live cover piles up: fund, fix dispatch, or accept paper-only               |
| `alert_risk_fallback_warn_pct` (`50`)                  | LLM fallback share since boot ≥ pct               | Model down/degraded: entries run heuristic; check endpoint                                |
| `alert_giveup_burst_n` (`3`) + `alert_giveup_window_secs` (`3600`) | N GiveUps in the trailing window    | Systemic dispatch trouble (RPC/Jito/congestion): stop funding more before cause found     |
| (always on) tip-floor fetch warn                      | Jito oracle unreachable (≤1 warn/60s TTL)         | Base tip rules unmodified: underland risk under congestion; check `tip_floor` vs landing  |

The heartbeat card already carries `Diverged` and `Pending buys`
counts every 6h, and listener `Dropped` proxies RPC 429 pressure.
A week nominal with no burst exits burn-in.

## 9. Secrets handling (read once, follow always)

Three secrets: `SOLANA_PRIVATE_KEY` (moves money), `INFERENCE_API_KEY`
(bills your LLM account), and `?api-key=` inside RPC URLs (bills your
RPC account). `YELLOWSTONE_TOKEN` (geyser auth+billing) follows the
same rules.

**Bot guarantees (coded + tested):**
- Secrets enter via env only, never via `config.toml` (no file mapping exists).
- Keypair buffers are zeroized; both secrets are scrubbed from process env right after load.
- Secrets never reach logs: bearer redacted, URL queries stripped, keypair has no `Debug`.

**Operator rules (the bot cannot do these for you):**
- Never `export SECRET=...` inline — it lands in shell history AND in
  `/proc` environ + `ps e` output, which the daemon child inherits.
  Prefer the `*_FILE` variants (`SOLANA_PRIVATE_KEY_FILE`,
  `INFERENCE_API_KEY_FILE`, `YELLOWSTONE_TOKEN_FILE`): 0600 files
  the bot reads once at boot. Fallback: `read -s`, a `0600` env file,
  or the OS keychain.
- Scope RPC keys per use, with IP allowlists where offered. A key pasted into `config.toml` makes that file a secret: keep `~/.pump-sentry/` `0700` (new config files are `0600` from birth).
- Keep inference on localhost; a remote plain-http endpoint with a key set raises a `cleartext` boot warning — treat it as a finding (live refuses it outright).

**Rotation:**
1. RPC/inference keys: mint new, swap env, restart, delete old. No funds move — 5 minutes.
2. Trading keypair (suspected leak): generate a new one OFFLINE, fund it per §2, point `SOLANA_PRIVATE_KEY` at it, exit/drain the old wallet, abandon it. Never reuse a leaked keypair.

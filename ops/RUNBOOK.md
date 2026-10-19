# Live operations runbook — pump-sentry

Paper mode needs nothing here. Live mode (`--live` / `IS_LIVE=true`)
moves real SOL: do every item in order, skip nothing.

## 1. Pre-flight checklist

Tunables live in `~/.pump-sentry/config.toml` by category (auto-created; the file is the only source, secrets stay in env).

Items come in two kinds: **boot-enforced** ones stop live boot (the
process exits with the named error until they pass); **operator** items
are checks only you can make — skipping one means unproven live
capability, not a refusal.

**Boot-enforced — live start refuses until these pass:**

- [ ] `max_daily_loss_usd>0` — day-loss breaker (exits always work); the
  tally persists in `day-state.json`, so a restart never resets it.
- [ ] `sim_strict=true` — unproven sims refuse instead of warn-and-proceed.
- [ ] `jito_tip_lamports>0` — tipless bundles may never land.
- [ ] `commitment=confirmed|finalized` — `processed` is refused (reorgable prints).
- [ ] `quote_usd` unset — static fallback is paper-only and refused live.
- [ ] Key file `0600` — `SOLANA_PRIVATE_KEY_FILE` must not be group/other
  readable; boot refuses it (`chmod 600 <file>` is the fix). Prefer the
  file over `SOLANA_PRIVATE_KEY` (env leaks into `/proc`, `ps`, children).
- [ ] RPC and Jito serve the same cluster — boot refuses when both URLs
  identify a cluster and they differ (devnet RPC + mainnet Jito can never
  land); unidentifiable URLs only warn, so check them yourself.

**Operator — run before funding; the bot cannot verify these:**

- [ ] `hot_wallet_max_sol` set — optional cap: buy preflight refuses while
  the wallet holds more (boot only warns). Fund the wallet at or below the
  cap (see §2), sweep any excess, and never raise the cap to "make the buy
  fit".
- [ ] `mainnet_v3_17_simulates` GREEN on this exact binary
  (`MAINNET_LAYOUT=1 cargo test -p executor --test mainnet_layout_acceptance -- --ignored`).
- [ ] `mainnet_bundle_acceptance` GREEN (Jito base64 wire proof, $0 probe:
  `JITO_ACCEPTANCE=1 cargo test -p executor --test jito_acceptance -- --ignored`).
- [ ] `raydium_authority_and_bundle_simulate` GREEN (post-migration exit proof:
  `RAYDIUM_ACCEPTANCE=1 cargo test -p executor --test raydium_authority_acceptance -- --ignored`).
  All three are simulation-only ($0 by construction); rerun on router-dominated
  samples — resampling is the procedure, not a failure. Set `MAINNET_RPC_URL`
  in the shell to give the probes a dedicated RPC (they dial it when set;
  the bot itself reads its RPC from `config.toml`).

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

`hot_wallet_max_sol` (optional) sits **above** `required_native`: it is
the ceiling that bounds what a leaked key or an un-swept wallet can
lose, not a funding floor. Buys refuse above it (boot warns); exits and
`sell` never consult it.

## 3. Start / stop / inspect

```sh
pump-sentry start --live   # daemon; state in ~/.pump-sentry/
pump-sentry status          # running? (pid, mode, log; flags a pause)
pump-sentry logs -n 50      # tail; --follow to follow
pump-sentry sell <mint>     # manual full exit now (only way parked bags leave)
pump-sentry pause           # kill switch: halt new entries (see below)
pump-sentry resume          # clear the kill switch
pump-sentry stop            # SIGTERM, 30s drain, snapshot, SIGKILL past 45s
```

`sell` follows daemon mode: it exits the live bag in live mode, and
closes the paper position in paper mode (cards always state the mode).

`sell <mint>` files drain every 15 gate rounds; the request file is
deleted only after the live leg settles (sold / manual-parked /
nothing held). A kept file is normal while the live leg still holds.

### Kill switch: stop new entries without stopping the bot

```sh
pump-sentry pause           # write the marker; entries halt within one gate round
pump-sentry status          # "entries PAUSED ..." confirms it
pump-sentry resume          # remove the marker; entries re-arm
```

What `pause` does (and does not do):

- Halts **every new open**: the check is the first step of the single
  open choke point, so no path (confirm flow, re-buy, override) slips
  past it. The marker is polled every gate round (~2s); no restart.
- Open positions keep managing: TP/SL/trailing/timeout, runner and
  moonbag scale-outs, the sell resend guard, parked-bag recovery, and
  `sell <mint>` all keep running. A held bag still stops out normally.
- The watch/audit/confirm pipeline keeps running; candidates that
  confirm while paused are skipped at the open (not queued).
- All other limits stay in force: `notional_usd`, `max_open_positions`,
  `max_exposure_usd`, `max_daily_loss_usd`, `max_expected_entry_gap_usd`,
  `hot_wallet_max_sol`.
- The marker survives `stop`/`start`/`restart` and crashes: the bot
  comes back paused until `resume`. If the state dir is lost (fresh VM,
  `--purge`), it comes back **unpaused** — check `status` after a
  rebuild or reinstall.

### Separately: the automatic data-degrade pause

Besides your marker, the bot pauses entries by itself when a critical
source is unhealthy — dead RPC pool, lost SOL/USD price, banned
ingestion, or a stream silent 30 minutes. Same choke point as the kill
switch (exits, scale-outs, and `sell <mint>` keep running), separate
state: one `DATA DEGRADED` card names the reason and recovery clears it
automatically. `status` does not show it (marker-file only) — read the
heartbeat card and the log edge lines. Do not `resume` against it: there
is no marker to clear. Fix the named source (RPC endpoints, token/quota,
feed) and entries re-arm on their own; a boot that never priced gets a
5-minute grace before this trips.

### Separately: managing open positions while paused

The switch governs entries only; these are the steps for what is
already held.

1. Watch them: `pump-sentry logs --follow` — exits keep printing and
   closing positions; this IS management, not a stalled bot.
2. Force one out: `pump-sentry sell <mint>` (full exit, mode per the
   daemon; drains in ~15 gate rounds). Repeat per mint.
3. Verify: expect the mint's ATA balance to go to zero; the WAL retires
   its tracked line only after the live leg settles (§4/§5). A parked
   manual note lingers by design until the bag is really gone.
4. No-market bag: if the pool holds no liquidity, nothing can sell at
   any price — the bot keeps the bag, books no fill, and holds heat.
   Exit by hand when a market returns (the `sell` request stays queued).
5. Then decide: `resume` (limits still apply, so a busted day or an
   over-cap wallet still blocks entries) or `stop` (a stopped bot runs
   **no** management either — only stop for maintenance, and restart
   before leaving positions unattended).

## 4. Reading state (shutdown snapshot + WAL)

- `~/.pump-sentry/shutdown-<ts>.json`: `paper_open`, `live_tracked`
  (base mints — suffixed `#2`/`#late` duplicates collapse here),
  `watchlist_pending`, `clean`. `clean:false` means fills are UNKNOWN —
  look every tracked mint up on the explorer before restarting.
- `~/.pump-sentry/day-state.json`: the day-loss tally (`schema`,
  `day`, `day_net`, `halt_alerted_day`), rewritten atomically
  (owner-only) after every closed trade and seeded at boot — a restart
  never resets the day. Delete it ONLY to deliberately forgive today's
  loss (entries re-arm and the day's halt card can fire again). A
  corrupt/missing file degrades to a fresh day, loudly in the log; a
  file from a NEWER schema version is refused the same way (upgrade the
  binary instead of guessing its numbers).
- `~/.pump-sentry/entries-paused`: the kill-switch marker. Presence =
  no new entries (see §3). It is not a lockfile: removing it by hand
  resumes entries at the next gate round, same as `pump-sentry resume`.
  The stat fails closed: a marker we cannot read (permissions, I/O)
  counts as present — fix the file, don't route around it.
- `~/.pump-sentry/live-wal.jsonl`: append-only custody log, fsync'd
  per line. **Never delete it** unless every tracked/manual bag is
  verified exited on-chain; boot replays it into pending/positions/
  manual. Corrupt lines are skipped loudly (`skipped` count). An
  unreadable WAL refuses live boot outright (custody unknown); a write
  failure (`live WAL WRITE FAILED` in the log) latches new entries off
  — exits and `sell <mint>` keep running — until the file is fixed
  (permissions/disk) and the bot restarts. Every boot stamps one `meta`
  line (schema + writer version, provenance only) and compaction keeps
  every stamp, so the file names all its writers. Every 30 minutes live
  mode additionally re-checks WAL claims per mint against the chain ATA
  balance (read-only): a mismatch warns and pages — verify on the
  explorer, never auto-correct.
- Boot ends with a per-mint chain check (`verified/unbacked/handed_off/
  excess/unreadable/awaiting_confirm` in the replay log): a mint whose
  replayed claims (`tracked + parked`) exceed the wallet's ATA balance
  is suppressed in memory — no auto-exit at an unverified size — and
  gets ONE manual note at the **chain** amount while no note covers it
  yet (`boot-balance-mismatch`; the WAL lines stay untouched). Claims
  below the balance only warn (unexplained excess); a mint with an
  unconfirmed buy is skipped until that recheck resolves. The check
  never writes the claim file, so it repeats identically every boot
  until the bag is exited or the file is retired.
- `~/.pump-sentry/journal.jsonl` is the decision journal
  (analytics-only): every recorded tick/open/exit/dispatch/gap/equity
  the replay and paper-vs-live metrics are derived from. It is
  fail-soft — recording degrades on write failure with a heartbeat
  warn (`decision journal is degraded`); trading is unaffected.
  `pump-sentry replay` re-derives the recorded decisions offline
  (no lookahead); exit code 1 means a mismatch or an unverifiable
  step (a missing tick line in front of an exit) — never an assumption.
  Do not edit the file: it is the record of what happened.

## 5. Parked bags (`MINT`, `MINT#2-…`, `MINT#late-…`)

Give-ups, unknown fills, and boot chain mismatches park here and stop
automated probing — this is custody, not a bug. Exit each via
`sell <mint>`. Second fills park suffixed without clobbering the
first; review every suffixed bag on the explorer (bundle UUID in the
alert card).

## 6. Logs (no restart needed)

Install `ops/logrotate/pump-sentry` (rename + SIGHUP reopen —
lossless). **Never use `copytruncate`** (drops lines mid-rotation).
The supervisor also rotates ≥50 MB logs at start time as a backstop.

## 7. When something looks wrong

| Symptom                       | Likely cause                                                         | Action                                                            |
|-------------------------------|----------------------------------------------------------------------|-------------------------------------------------------------------|
| No entries for hours          | Gate quorums unmet / day-loss tripped / pause marker present / data-degrade auto-pause (no marker — see `DATA DEGRADED` row) / exposure or hot-wallet cap reached / public RPC throttled (drops) | `logs`, `status` (flags a pause marker, not the auto-pause), heartbeat card, use dedicated RPC |
| `DATA DEGRADED` card, entries paused without marker | RPC pool down / SOL/USD feed lost / ingestion banned / stream stalled 30 min | Fix the named source (endpoints, token/quota, feed); re-arms automatically — no marker to clear, do not `resume` against it |
| `diverged` alerts             | Live buy skipped/failed, paper uncovered                             | Review, fund, or accept paper-only                                |
| Exits stalled, SOL/USD stale  | Price feed down (DexScreener; Jupiter retired)                           | By design (no static fallback); exits resume on recovery          |
| `zero-liquidity tick` warns   | Curve/pool drained (a tick carries parsed reserves, so this is a real empty market, not a read miss) | By design: no fee-only fiction for a stop-loss. The bag stays held (paper and live, heat stays occupied, breaker books nothing) and exits resume on a priced tick; migration closes bypass. Last resort: `sell <mint>` |
| `MANUAL REVIEW REQUIRED`      | Unknown fill / retries exhausted                                     | Explorer first, then `sell <mint>`                                |
| `INGESTION BANNED` card       | Geyser 429s for ~30 min straight (quota/exhaustion, not burst)       | **Stop the bot.** Check token/quota/dashboard; wait out the cycle or upgrade tier. To keep running blind-free: clear `yellowstone_endpoint` (WS mode) and restart. Retries hourly on its own; post-recovery backfill is capped (geyser: slot cap, shed counted; WS: 3 signature pages, capped remainder counted lost) — read the heartbeat gap lines for what was actually missed |
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

The heartbeat card every 6h carries `Diverged` and `Pending buys` plus
the health view — RPC pool (`n/m ok · failovers`, inconsistency flag),
data-degrade state, oldest open age, stream gaps shed (slots + capped
backfills), and windowed GiveUps — and listener `Dropped` proxies RPC
429 pressure. A position overdue past its own timeout warns log-only.
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
- Live boot **refuses** a `SOLANA_PRIVATE_KEY_FILE` with any group/other
  permission bit (checked before a byte is read; the error names the
  `chmod 600` fix). A world-readable key is treated as already leaked.

**Operator rules (the bot cannot do these for you):**
- Never `export SECRET=...` inline — it lands in shell history AND in
  `/proc` environ + `ps e` output, which the daemon child inherits.
  Prefer the `*_FILE` variants (`SOLANA_PRIVATE_KEY_FILE`,
  `INFERENCE_API_KEY_FILE`, `YELLOWSTONE_TOKEN_FILE`): 0600 files
  the bot reads once at boot. Fallback: `read -s`, a `0600` env file,
  or the OS keychain.
- Keep the hot wallet funded at or below `hot_wallet_max_sol`: that cap
  is the blast radius if the key leaks. Buys refuse above it — sweep the
  excess, don't raise the cap.
- Scope RPC keys per use, with IP allowlists where offered. A key pasted into `config.toml` makes that file a secret: keep `~/.pump-sentry/` `0700` (new config files are `0600` from birth).
- Keep inference on localhost; a remote plain-http endpoint with a key set raises a `cleartext` boot warning — treat it as a finding (live refuses it outright).

**Rotation:**
1. RPC/inference keys: mint new, swap env, restart, delete old. No funds move — 5 minutes.
2. Trading keypair (suspected leak): generate a new one OFFLINE, fund it per §2, point `SOLANA_PRIVATE_KEY` at it, exit/drain the old wallet, abandon it. Never reuse a leaked keypair.

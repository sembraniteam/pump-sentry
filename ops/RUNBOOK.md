# Live operations runbook — pump-sentry

Paper mode needs nothing here. Live mode (`--live` / `IS_LIVE=true`)
moves real SOL: do every item in order, skip nothing.

## 1. Pre-flight checklist (boot refuses without these)

- [ ] `MAX_DAILY_LOSS_USD>0` — day-loss breaker (exits always work).
- [ ] `SIM_STRICT=true` — unproven sims refuse instead of warn-and-proceed.
- [ ] `JITO_TIP_LAMPORTS>0` — tipless bundles may never land.
- [ ] `COMMITMENT=confirmed|finalized` — `processed` is refused (reorgable prints).
- [ ] `QUOTE_USD` unset — static fallback is paper-only and refused live.
- [ ] `mainnet_v3_17_simulates` GREEN on this exact binary
  (`MAINNET_LAYOUT=1 cargo test -p executor --test mainnet_layout_acceptance -- --ignored`).
- [ ] RPC and Jito serve the same cluster (boot checks URL substrings;
  devnet RPC + mainnet Jito can never land).

## 2. Funding math (native SOL first)

SOL-pair v3 buys transfer **native SOL** (official BUY.md/TRADE_V3.md).
Fund native for everything, no WSOL float needed for SOL pairs:

```
required_native = MAX_OPEN_POSITIONS × (NOTIONAL_USD/SOL_USD × (1+SLIPPAGE_BPS/10000) + fees)
                + JITO_TIP_LAMPORTS × MAX_OPEN_POSITIONS × 2   // buy + sell legs
                + 0.05 SOL reserve (hard floor, never spent)
```

A WSOL float funds **token-paired quotes only** (USDC/pump coin,
debited via `TransferChecked`). Boot fails closed below tip+reserve
and warns loudly on zero WSOL.

## 3. Start / stop / inspect

```sh
pump-sentry start --live   # daemon; state in ~/.pump-sentry/
pump-sentry status          # running? (pid, mode, log)
pump-sentry logs -n 50      # tail; --follow to follow
pump-sentry sell <mint>     # manual full exit now (only way parked bags leave)
pump-sentry stop            # SIGTERM, 30s drain, snapshot, SIGKILL past 45s
```

`sell <mint>` files are drained every 15 gate rounds; the request file
is deleted only after the live leg settles (sold / manual-parked /
nothing held). A kept file is normal while the live leg still holds.

## 4. Reading state (shutdown snapshot + WAL)

- `~/.pump-sentry/shutdown-<ts>.json`: `paper_open`, `live_tracked`
  (base mints — suffixed `#2`/`#late` duplicates collapse here),
  `watchlist_pending`, `clean`. `clean:false` (or a second SIGTERM
  force-abort) means fills are UNKNOWN — look every tracked mint up
  on the explorer before restarting.
- `~/.pump-sentry/live-wal.jsonl`: append-only custody log, fsync'd
  per line. **Never delete it** unless every tracked/manual bag is
  verified exited on-chain; boot replays it into pending/positions/
  manual. Corrupt lines are skipped loudly (`skipped` count).

## 5. Parked bags (`MINT`, `MINT#2-…`, `MINT#late-…`)

Give-ups and unknown fills park here and stop automated probing —
this is custody, not a bug. Exit each via `sell <mint>` (resurrects
the bag for one explicit exit). Second fills park suffixed without
clobbering the first; review every suffixed bag on the explorer (bundle UUID in the alert card).

## 6. Logs (no restart needed)

Install `ops/logrotate/pump-sentry` (rename + SIGHUP reopen —
lossless). **Never use `copytruncate`** (drops lines mid-rotation).
The supervisor also rotates ≥50 MB logs at start time as a backstop.

## 7. When something looks wrong

| Symptom                       | Likely cause                                                         | Action                                                            |
|-------------------------------|----------------------------------------------------------------------|-------------------------------------------------------------------|
| No entries for hours          | Gate quorums unmet / day-loss tripped / public RPC throttled (drops) | `logs`, heartbeat card, use dedicated RPC                         |
| `diverged` alerts             | Live buy skipped/failed, paper uncovered                             | Review, fund, or accept paper-only                                |
| Exits stalled, SOL/USD stale  | Jupiter feed down                                                    | By design (no static fallback); exits resume on recovery          |
| `MANUAL REVIEW REQUIRED`      | Unknown fill / retries exhausted                                     | Explorer first, then `sell <mint>`                                |
| Duplicate cards after restart | Crash between dispatch and WAL                                       | WAL replay is conservative; verify on-chain, exit dups via `sell` |

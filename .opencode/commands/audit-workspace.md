---
description: Run the full HIGH→LOW workspace audit (fees, RPC, LLM, RugCheck, v3, Raydium, hygiene) and score simulator/live readiness.
---

Run a full audit of this repo in severity order (HIGH → MEDIUM → LOW → stale/hardcode/deadcode/tests/validation),
mirroring the audit that produced the current hardening. Do NOT change code unless asked — report findings with
`file:line` evidence, severity, and a 0–10 readiness score for simulator vs live.

### Audit checklist (one section per area, with verdict + evidence)

1. **Fees / slippage / tips (dynamic?)** — `crates/simulator` (`market.rs` `try_*` vs capped, `portfolio.rs`
   `total_dispatch_usd`/`realized_net_pnl`/`CurveExhausted`/SL bounds), `src/live.rs` slippage guards (≥100% rejected
   both sides?), `src/gate.rs` `fee_aware_sl_floor` (clamped? commission included?),
   tier-`None`/liquidity-0 handling. Is any PnL headline hiding dispatch bleed?
2. **RPC waste** — `screen_developer` early-reject before fan-out? `DEV_SIG_LIMIT` ≥ sample? truncated-window /
   err-majority fail-closed? bundle buyer selection earliest-first? monopoly over ALL creation buys?
   hunt budget (progress vs blip burn)? `sweep_idle` timer-wired?
3. **LLM** — malformed PASS holds PASS (`engine.rs`)? SL floor −8 == `MIN_STOP_MAGNITUDE_PCT` == prompt?
   `fee_aware_sl_floor` clamped to −100? bundle fields threaded (`creation_slot_buys`/`cluster_pct`)?
   timeout vs invalid counters split? `INFERENCE_TIMEOUT_MS=0` honest? preamble stripped?
4. **RugCheck** — streaming 1MB cap? `Deserialization` terminal? transient set (timeout/connect/body/5xx/429/408)?
   `fetch_report` private? policy toggles test-only? README accurate?
5. **Pump v3 / Jito / Solana** — dispatch v3-only 17-account (`build_buy_v3_ix`/`build_sell_v3_ix`,
   `BuyV3Accounts`/`SellV3Accounts`)? No retired 12/18/27/26 builder re-added? `min_sol_output==0` rejected?
   `{"encoding":"base64"}` present? per-mint ATA program? `SIM_STRICT` on live? Any `build_swap_ix` /
   `SwapAccounts` / Global-41..73 / creator-vault reference creeping back in?
6. **Raydium migration** — WSOL-base (not quoted)? owner + mint pairing gates? unvalidated hits → `None`+retry (never
   pool-less Raydium)? `follow_pool` refuses pool-less loudly?
7. **Hygiene** — deadcode (`grep` before claiming), over-`pub`, duplicated constants, endpoint literals,
   stale tests/fixtures, validation gaps, naming drift, error/log inconsistencies.

### Scoring (be honest)

- Simulator readiness 0–10 (paper-backtest honesty: dispatch-netted? curve-exhaust rejected?).
- Live readiness 0–10 (funds-ready? list blockers in order: fee model, PASS-hold, floor guards,
  fallback promotion, prompt context, model hit-rate, RPC spike budget).
- Confirm v3 dispatch completeness explicitly (or name the gap).

### Reference docs (strong defaults, NOT ground truth)

Skills under `.opencode/skills/` describe intent; wire formats must be proven against mainnet replays/simulations.
Known past drift (do not regress): v3 vs legacy shapes, WSOL-base orientation, `MIN_STOP` −8, SL clamp −100,
deserialization terminal, PASS-hold. `cargo test --workspace` + `cargo clippy --all-targets` (zero warnings)
is the gate — quote results.

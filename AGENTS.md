# AGENTS.md — pump-sentry

Paper-trading Solana memecoin sniper (Pump.fun → Raydium). Rust workspace:
binary (`src/`) + 9 crates (`crates/*`). Default mode is paper-only.

## Safety (read first)

- **Never run with `--live` / `IS_LIVE=true`, never fund a wallet, never
  send transactions.** Live paths move real SOL via Jito.
- Tests hitting real mainnet exist and are `#[ignore]`d behind env gates
  (`JITO_ACCEPTANCE=1`, `MAINNET_LAYOUT=1`, `RAYDIUM_ACCEPTANCE=1`; the
  creatorless-fee probe also needs `PUMP_ACCEPTANCE_POOL` and optionally
  `PUMP_ACCEPTANCE_HOLDER`, and the probes dial `MAINNET_RPC_URL` when set).
  Never run them casually;
  only on explicit user request. They cost $0 by construction (unfunded
  payers, simulation-only) — keep it that way. Raydium probes resample
  (router-dominated blocks yield zero direct swaps); rerun, don't "fix".
- No secrets in repo. Preflight/simulate before any dispatch change.

## Verify (in this order)

```sh
cargo test --workspace            # 569 passed; 6 live probes stay ignored
cargo clippy --all-targets        # must be zero warnings
cargo fmt --all -- --check        # repo has PRE-EXISTING diffs: fix only
                                  # your own lines, never whole-tree fmt
```

- Single crate: `cargo test -p <name>`.
- After any behavior change, re-run workspace tests + clippy. Both were
  zero at last commit; keep them zero.

## Architecture that filenames don't tell you

- `crates/ids` is the single source of truth for on-chain constants (program IDs, discriminators, fee/volume addresses).
  A program upgrade
  edits exactly one file there — never duplicate a constant elsewhere.
- `crates/curve/src/fee.rs` is the single source for the Pump.fun fee
  schedule: decode, tier pick, and the creator-leg rule (`creator ==
  default` ⇒ creator fee 0). `executor::quote` and the paper fee view
  both delegate — a second copy once charged paper a creator fee the
  chain never charges (live sell floor looser than the dispatch quote).
- `src/pipeline.rs` (923 lines, ~640 of code; tests live in
  `src/pipeline_tests.rs` via `#[path]`, same crate so `pub(crate)`
  stays private) is the orchestrator:
  gate → risk → paper ledger → live mirror. `LiveExecution`/`live_mirror` (live
  dispatch + `FillGapTracker` calibration), `gate` (watchlist + opens), and `DailyBreaker` are split
  out; method bodies live on `Pipeline` via those modules. Do NOT split
  further (coherent as-is; 10 single-impl traits are a deliberate
  testability trade-off, not tech debt to "fix").
- `src/live.rs` sizes/orders live trades; `crates/executor` builds and
  sends them. Paper ledger is authoritative; live legs only mirror it.
  Custody types are `Pubkey`, not `String` (`LivePosition`, `PoolLeg`,
  `LiveBuyParams`/`LiveSellParams`): parse ONCE at the caller boundary
  (`live_buy`, `follow_pool`, WAL replay), never re-validate downstream.
  `String` survives only at RPC/feed boundaries (`ReserveFeed`,
  map keys, logs). `LiveBuyParams` carries explicit `decimals: u8` too.
  Test addresses: `crate::live::TEST_*` consts (+ `test_pubkey(seed)`);
  new test fixtures must use them, never arbitrary strings.
- Paper quantities are `u64` whole tokens (absolute; direction from
  `side`), and the ledger debits/invests the floored size, never the
  nominal ticket. Tests asserting quantities use integers; paper↔live
  size parity holds only to sub-token dust (assert bounded, not exact).
  Partial slices size through `simulator::slice_quantity` (floored,
  never rounded; NaN/≤0 → 0, ≥100% → whole): the paper book and the
  live sell mirror share it so one scale-out cannot size two ways.
- Watchlist hard cap evicts by `PendingSignal::intake_secs` (oldest
  intake first), never by a re-stamped audit time, so audit retries
  cannot change who gets shed.
- Paper entries/exits are state-based: `simulator::market`
  (`CurveReserves`, `entry_fill`/`exit_price`) is the single quote math,
  mirrored by `src/live.rs` (`plan_buy`/`plan_sell_floor`,
  `paper_mark_from_tick`) and parity-pinned against `executor::quote`
  (`paper_curve_math_pins_executor_quote`: single-fee tier bit-for-bit,
  split tier ≥ chain cost / ≤ chain net inside ≤ 1 lamport). Exact path
  only for `FillModel::BondingCurve` + Long + curve present; proxy math
  otherwise. A curve-draining ticket fails closed in paper (`None`) where
  the chain clamps to `real_tokens` — documented divergence.
- `simulator::Mark` (spot + book liquidity + optional curve reserves) is
  the ONE state carrier: `PriceTick.exec` feeds it, `LiveBuyParams` /
  `LiveSellParams` carry it, so gate, paper book, guard, and dispatch
  quote exactly the state a position was booked on. Migrated (`complete`)
  ticks price off stale spot in paper (documented boundary; live pool
  legs settle at vault reserves). `PortfolioSnapshot` carries
  `mark_value` + `realizable_value` (`exit_discount_usd` = impact+fee);
  paper closes log `realizable_vs_mark_bps` (plus the booked
  `book_mark`/`book_discount`), gate cards report the BOOKED entry,
  never the tick spot.
- Failed-dispatch costs are honest: only `DeadReason::FailedOnChain`
  burns base + priority fees (`failed_dispatch_cost_lamports`; the tip
  rolls back); `NeverForwarded`/`Expired` debit nothing.
- Never read `self.portfolio` inside a match scrutinee or call arguments
  on live paths: `live_buy` hoists `venue_fee_bps` into a local BEFORE
  the match — a scrutinee temporary lives through every arm and deadlocked
  the pipeline once.
- `paper_dispatch_legs_usd(tip, priority, sol_usd)` takes 3 args (priority
  leg included; calm-weather floor, documented). Typed units live in
  `src/money.rs` (`Bps`/`Percent`) with `Settings::slippage()` /
  `runner_moonbag()` accessors — use them in new validation, don't add
  parallel float checks.
- Risk flow: `risk-engine` profile → `strategy` rewrite (`adaptive`/`scalp`/`runner`) → `simulator` exits
  (TP/SL/trailing/
  timeout/moonbag scale-outs with breakeven ratchet + timeout lift).
- Risk limits (all optional, `0` = off; none of them ever blocks an
  exit): `notional_usd` (one ticket), `max_open_positions`
  (concurrent; live-held mints count), `max_exposure_usd` (paper cost
  basis + one ticket per unpaired live bag — bags store no USD basis,
  that estimate is deliberate and mirrors heat), `max_daily_loss_usd`
  (realized day loss), `max_expected_entry_gap_usd` (proven per-ticket
  entry bleed from `FillGapTracker::expected_entry_gap_usd`; unknown
  cells always allow, never refuse), `max_top_holder_pct` (top-10 supply
  share from `Screener::top_holder_pct`; unreadable refuses while armed),
  `min_fill_rate_pct` (trailing buy-fill trip, thin windows unknown),
  `max_avg_exit_gap_usd` / `max_avg_entry_gap_usd` (windowed paper-vs-live
  gap trips; the tracker totals now follow the retained window so the
  average stays windowed), `hot_wallet_max_sol` (buy preflight refuses
  above it, off the same balance read as solvency; never on sells).
  The day tally persists in `day-state.json` (`src/day_state.rs`: atomic
  tmp+rename via `cli::atomic_write_private`, `#[serde(default)]` fields,
  missing/corrupt = fresh day loudly) and is seeded at boot — a restart
  must never reset a limit (live custody already outlives restarts via
  the WAL). Paper's book is in-memory by design; the tally is not.
- Kill switch = `entries-paused` marker file, polled every gate round
  by `run_pipeline` (edge-logged via `Pipeline::set_entries_paused`)
  and checked FIRST in `open_tracked` — the single entry choke point, so
  confirm/re-buy/override paths all pass through it. Entries halt;
  exits, runner/moonbag scale-outs, the sell resend guard,
  parked-bag recovery, and `sell <mint>` keep running, and the marker
  survives restarts (`pause`/`resume` in `cli.rs`). Do NOT add a second
  pause check site; a marker left behind must fail closed (paused), and
  `status` reports it. The marker stat itself fails closed too
  (`cli::marker_present`: only NotFound = not paused; any other stat
  error keeps entries halted).
- `SOLANA_PRIVATE_KEY_FILE` must be owner-only: `load_from_env` refuses
  any group/other permission bit BEFORE reading (a world-readable key is
  a leaked key). `hot_wallet_max_sol` bounds what a leak can lose.
- Serde parsing goes through DTOs everywhere (wire, fixtures, WAL): test
  fixtures decode with the same DTOs as production. Shared helpers live in
  `crates/{executor,listener,migration}/src/test_fixture.rs`;
  `crates/executor/tests/common/mod.rs` mirrors them for integration
  targets (`cfg(test)` items are invisible across the lib→integration
  boundary) — deliberate duplication, don't "dedupe".
- `src/wal.rs` `WalLine` (tagged `t`, snake_case) is the compat-critical
  replay format: strict decode — a line missing a load-bearing field or
  carrying a wrong type is counted in `skipped`, never half-adopted; older
  shapes stay readable via `#[serde(default)]` fields. Replay/compaction
  changes are data-loss-sensitive. `pending` lines carry an optional
  `blockhash`; `sell_pending` an optional `sig` + `blockhash` (all
  defaultable): they are the reconcile/expiry evidence the guards
  re-read at boot. Lines without them (pre-upgrade) can never prove
  expiry: they park to manual instead of dropping. Do NOT add replay
  netting for `manual` lines: parking is a runtime-map move, and the
  same mint can legitimately hold a parked bag AND a separate re-buy
  bag, so key/amount-based netting can silently delete tracked
  custody (and compaction writes positions before manual). The parked
  note lingers by design; `take_manual` retires it. Boot chain
  check: `reconcile_boot_custody` (main.rs) sums each mint's replayed
  claims (tracked + parked manual, per mint — the ATA is per mint,
  not per bag) and reads it ONCE
  (`SwapExecutor::observe_token_balance`; `Ok(None)` = no ATA = zero
  held). Claims == chain keeps both (a parked bag plus a real re-buy
  is one such sum); claims < chain keeps both and warns (unexplained
  excess); claims > chain suppresses the tracked copy in memory —
  never auto-exit an unverified size — and hands off at most ONE
  manual note at the CHAIN amount, only while no note covers the mint
  (repeated boots must never stack claims; the WAL lines are never
  rewritten by the check — the file is the claim of record and
  suppression is idempotent). Mints with an unconfirmed buy are
  skipped (the late-confirm recheck owns that claim). An unreadable
  balance warns and keeps the claims (dispatch preflights still fail
  closed). Mid-run drift needs no periodic job: every dispatch
  preflights the chain (`preflight_buy` solvency; `held >= amount` in
  `preflight_sell`, `preflight_sell_raydium`,
  `preflight_sell_pumpswap`). SOL is reconstructed by the funding
  report at boot + per-dispatch solvency, not by the WAL.
- The WAL is also the fail-closed read/write boundary: `replay_file`
  returns `Err` for an unreadable file (only NotFound = empty) and live
  boot REFUSES (`replay_live_wal` in `main.rs`) instead of starting with
  unknown custody; any write failure (open/append/fsync/serialize)
  latches `wal_degraded` (`WalSink`, shared across `LiveTrader` clones)
  and `open_tracked` refuses new entries next to the kill switch until
  an operator restart with a working WAL — exits, pending rechecks, and
  `sell <mint>` keep running. State files stage through
  `cli::atomic_write_private` (owner-only tmp + rename).
- The decision journal (`src/journal.rs`, `journal.jsonl` beside the log)
  is the REPLAY/analytics record and is fail-SOFT — the opposite policy
  from the WAL: a write failure latches `degraded` + prompts the
  heartbeat warn (`is_degraded`) and skips recording; trading never
  stops for it. Hooks record inputs BEFORE decisions (tick lines carry
  the mark each check ran on; Open carries the open mark + risk
  provenance; Exit carries the tick mark + recorded trade), so offline
  replay (`src/replay.rs`, `pump-sentry replay`) re-derives the paper
  book via the same public portfolio APIs and FAILS CLOSED on missing
  inputs: an exit without its causal tick is "unverifiable" and exit
  code 1 — never assumed. Metrics are two strict buckets (paper
  simulator vs live chain outcomes) with the paper≠live-evidence
  disclaimer printed; do NOT merge them or re-derive gap math outside
  `FillGapTracker` (journal Gap lines mirror its `Option<FillGap>` /
  `Option<EntryGap>` returns, single owner). Live money-debits replay
  from the recorded `debit_usd` (first-touch / FailedOnChain-only rule
  mirrors the runtime — the journal records what was debited, it never
  recomputes costs). Two accounting pins: Open carries the ACTUAL
  ticket (`ticket_usd`; a delta from the Boot snapshot is a NOTE, not a
  mismatch), and SellAttempt carries `settle_paired` so a paired
  settle's tip counts exactly once (inside its exit Gap line, tracker
  math) and only UNPAIRED debits are subtracted from the estimate.
  Trigger provenance is a pin too: Tick carries `manual_exits` for
  operator sells (`sell <mint>` books Manual with no market trigger —
  replay re-derives them as Manual only via that flag; a Manual exit on
  a flagged-less market tick is a mismatch, never excused), and every
  live buy gets exactly ONE outcome line (the direct-dispatch path
  emits Attempt + confirmed Outcome; the Attempt owns the debit, the
  Outcome owns the verdict). The TP scale-out fraction math is single
  owner `simulator::market::exit_fraction` (tick path + replay both
  call it) with `PaperPortfolio::moonbag_of`.
- Confirm-tape microstructure (`listener::screen::micro`, `MicroSignals`):
  buys/sells, buy ratio, base-unit volumes, unique traders, top-wallet
  share, insider net flow, round-over-round velocity — pure computation
  over already-observed swaps, no new RPC. Threaded through the confirm
  path (`PendingSignal::prev_tape` supplies the velocity history of one)
  into `JournalLine::Open.micro` (optional; pre-signal lines decode as
  `None`, and replay joins exits to entries per boot, never across
  restarts). Replay bins paper expectancy per signal with chronological
  halves and a thin-bin floor (`MICRO_BIN_MIN`); bins are analysis
  display, never trading logic. Deliberately excluded: per-round
  funder graphs (creation-slot bundle screen owns related-wallet
  forensics) and liquidity change (rounds observe swaps, not reserves).
- Ticks with non-positive/NaN liquidity are unpriceable
  (`unpriceable_liquidity`): a delivered tick carries parsed reserves
  (the ticker clamps non-positive to 0), so this is a drained market
  or an unusable pool — NOT a missing read. `Pipeline::on_tick` then
  defers BOTH sides: paper exits (a stop-loss must never book a
  fee-only fill into a drained pool) and the ids-empty live attempt
  (the curve floor cannot price it; the dispatch would burn a tip on
  a sell the chain must refuse; the ids-empty path sells the WHOLE
  remainder — a boot-restored bag has no paper to mirror, so its exit
  is all-or-nothing). The bag stays held, heat stays
  occupied, and the breaker books nothing until a priced exit — no
  market, no fill, by design. The notice is one warn per drained
  episode (per-mint mark, `report_drained`), debug while it lasts;
  migration closes bypass (a complete tick is a moved market, and its
  pool leg prices off vault reserves). A pending buy pins the mint's
  watch (`PendingStore::has_pending_for` via `maybe_unwatch`):
  late-confirm rechecks ride that mint's ticks, so dropping the watch
  would strand the buy unresolved.
- Skills are strong defaults, **not ground truth**:
  the Jito skill once omitted the mandatory `{"encoding":"base64"}`
  param and every bundle was refused (`-32602`). Wire formats must be
  proven against mainnet replays/simulations, never trusted from prose.
  (There is no `.opencode/skills/` dir in this repo; comments naming
  `skill solana-*` refer to out-of-repo docs.)
- Alert burn-in defaults are ON (`alert_diverged_warn_n=3`,
  `alert_risk_fallback_warn_pct=50`, `alert_giveup_burst_n=3`): model
  trouble and divergence log a warn by default — log-only, never
  Discord cards (`ops/RUNBOOK.md` §8). Do NOT "fix" them back to 0.
- One shared `Arc<RpcClient>` per process (built in `main`, injected via
  `with_rpc` / `subscribe_with_rpc`). Never add a new `RpcClient`
  construction site; `new()` constructors are test/back-compat wrappers.
  That one client now wraps `executor::rpc_pool::RpcPool` (read-path
  failover + latency + slot-staleness/inconsistency sampler built in
  `main` from `http_url` + `fallback_rpc_urls`): still ONE construction
  site — the pool builds endpoint senders inside itself. Reads only.
  Attempt budgets are per-method (`attempt_timeout`: 6s light /
  25s block-weight) and sit UNDER the callers' 10s outers, so a hung
  primary actually fails over instead of dying at the outer first.
  **Dispatch never rides the pool and never fans out**: one decision →
  one Jito bundle on one endpoint; the failover sender re-attempts the
  NEXT endpoint only AFTER an attempt failed (failover, never an
  all-endpoints broadcast — a broadcast would double-execute and
  double-rate-limit). Do not add a second dispatch path "for
  redundancy" without written executed-once semantics.
  `RpcPool::degraded` is hysteresis-gated, never blip-gated: no probe
  evidence yet (boot) = healthy; sampler reaches nobody = degraded;
  all-reached-but-none-eligible counts ONLY when every endpoint is
  stale (a lone cooldown is a read-level fail-closed, not a pause).
  Pinned by `single_blip_is_not_degradation` /
  `boot_grace_withholds_degradation_without_probe_evidence`.
  Endpoint URLs are redacted in pool logs (`ids::redact_endpoint`):
  an `?api-key=` must never reach a log line. Mainnet proof is a
  read-only `#[ignore]` probe (`rpc_pool_acceptance`, `RPC_POOL_ACCEPTANCE=1`).
- Data-degrade pause (`data_paused` on `Pipeline`) halts entries through
  the SAME `open_tracked` choke point as the kill switch — do NOT add a
  second check site. The main-loop supervisor owns it (edge-logged +
  one card). Reasons: RPC pool degraded (sampler-confirmed, never a
  blip), SOL/USD feed stale — but a boot that never priced gets a
  300s grace (`has_ever_priced`; losing a HAD price pauses at once) —
  listener ingestion banned, plus a mode-agnostic stall trip (no logs
  for 30 min; the ban latch is geyser-only, pump.fun launches are
  seconds apart so silence is a dead stream). WS backfill pages the
  signature list (cheap) up to `BACKFILL_MAX_PAGES=3` seeking the seen
  boundary (`classify_backfill_page`, oldest-of-page rule); past the cap
  with unseen beyond it counts `backfill_pages_capped` (lost, unknown
  count — never silently). Execution-quality trips ride the same flag:
  buy fill rate below `min_fill_rate_pct`, windowed paper-vs-live gap
  worse than `max_avg_exit_gap_usd` / `max_avg_entry_gap_usd`, and the
  latched ledger-short verdict (refreshed only by a passing custody
  recheck, never by elapsed time). Exits and `sell <mint>` keep running;
  recovery clears the pause automatically. The operator's marker file
  and this flag stay separate states (operator vs system).
- WAL carries a per-boot `Meta` stamp (`wal::WAL_SCHEMA_VERSION`,
  `LiveTrader::wal_write_meta`, written after compaction; fresh boots
  stamp too): provenance only, no custody — old binaries skip it.
  Compaction preserves every stamp (`WalReplay::meta`); the file names
  ALL its writers, not just the last session.
- Curve completion events are decoded, not assumed
  (`listener::events`, frozen proofs `complete_mayhem_buy_mainnet.json`
  + `complete_std_buy_mainnet.json`): `TradeEvent` = curve part
  (tokens == buyer credit, balance-reconciled), `CompleteEvent` =
  transition. Both exhausting buys filled ENTIRELY from the curve (no
  pool part, no migrate in-tx) and both landed the remainder on
  EXACTLY 206,900,000,000,000 base units — completion is a token floor
  shared by mayhem and standard curves, not a SOL threshold. So no
  curve→pool split-fill math exists: over-remainder tickets stay
  fail-closed in paper (`Exhausted`) and dispatch (`DepletedCurve`),
  and the chain clamp (`tokens.min`) is the documented divergence, not
  a pool leg. Recent buys (both completing and ordinary) carry a
  trailing 25th data byte (`0x00` and `0x01` both observed live, normal
  fills historically 24) — an evolving wire field of unverified
  meaning, NOT an exhaust flag (165 non-completing 25-byte buys killed
  that theory); a later 400-block census also found 26-byte variants
  coexisting, with 24-byte fills still landing — our 24-byte builder
  keeps working and sims stay green, so nothing is stale. Do NOT add
  trailing bytes without a proven semantic, and treat a future 24-byte
  sim failure wave as the revisit signal. Over-remainder behavior is
  settled by the same census: 389 arg==delivered against ZERO partial
  fills, plus 42 `BondingCurveComplete` failures — oversize attempts
  FAIL on chain instead of clamping or splitting, so fail-closed
  (`Exhausted`/`DepletedCurve`) mirrors chain reality and the quote
  `tokens.min` is estimation convenience, not a fill path.
  `PostCompleteBuyEvent` is observed exactly once (Jupiter-routed
  buy crossing below the floor; user/mint/curve/timestamp pinned, tail
  opaque): even there every bought token came from the curve, so it
  marks a floor-crossing fill, not a pool leg — still no curve→pool
  split-fill math exists anywhere observed.
- The legacy `buy` discriminator also rides fee-routed 18-account
  fills (creator/mayhem flow: fee program + buyback wallet +
  mayhem-linked accounts; trader still at index 6, amount still first
  u64 — proven across 3 samples with the `TradeEvent` user cross-checked
  on each, and the amount equals the buyer balance delta to the unit).
  No new classifier arm was needed: the legacy arm already covers the
  wider layout, pinned by `replay_legacy_buy_18_mainnet` (+
  balance-delta twin) against future arm-tightening. (An earlier
  same-bytes "new discriminator" constant was reverted — the pairwise
  distinctness test caught the alias.)
- Three-way fee accounting (charged vs direct vs held), conservation-proven
  on a frozen mainnet buy (modeled cost+proto+creator == curve_delta +
  buyback_delta to the lamport): the TRADER is charged cost + protocol +
  creator (the full venue basis in every fill price, paper and live);
  the buyback recipient transfer ([13], SOL, in-tx) is carved from the
  protocol leg by the program — never an added charge, so no buyback
  bps exists anywhere in the math (rate holds universal 47.5 bps of
  event SOL, n=333 min==median==max; exactly half the 95 bps protocol
  leg on the proven sample); the held remainder (protocol rest +
  creator) sits in curve reserves, likewise inside the price. Nothing in
  the codebase debits recipient flows (no recipient-balance
  reconciliation exists by design); debits are only chain-paid overhead
  (tip/priority/base/rent) while venue costs ride fill prices. The
  mayhem sample paid zero buyback (all-held legs) with the model still
  exact — carved-when-present, never added. Do NOT add a buyback bps.
- Exact-quote fills count DELIVERED, not the floor: `min_out` understates
  whale fills by orders of magnitude (frozen PostComplete sample:
  min_out 1 vs 6.98T delivered), and undercounting attacker supply
  biases monopoly screens toward PASS. `extract_pump_swaps` lifts
  unambiguous exact-quote buys to the buyer+mint balance delta via
  `max()` — never below today's figure; shared/ambiguous deltas and
  balance-less txs keep `min_out` (pinned by
  `replay_exact_quote_lifts_to_delivered_delta`).
- Mayhem curves complete at the same token floor (proven: mayhem flag
  set, remainder exactly 206.9T like the standard sample — the earlier
  "~0.05 SOL" framing confused a coincidence for a threshold) and no
  pool migration was observed for the sample — no synthetic pool leg
  exists for them. Paper closes at the stale spot on the complete tick
  uniformly; live bags park after the retry budget and `sell <mint>`
  stays refused while complete. There is no better venue: the chain
  itself refuses post-complete curve sells (`BondingCurveComplete`
  6005, $0-sim proven), so the refusal (`state.complete → BadOrder`,
  pinned by `complete_curve_sell_refuses_without_dispatch`) mirrors
  chain policy and burns no tip.
  `day-state.json` carries `schema` (`day_state::DAY_STATE_SCHEMA`); a
  file from a NEWER schema degrades to a fresh day loudly, never
  adopts guessed numbers. Crash-recovery rules are pinned by tests:
  torn final line / garbage lines are `skipped`, never half-adopted;
  unreadable WAL still refuses boot.
- Resend guard (`prior_sell_pending`) decides through the SAME
  `resolve_dispatch` as buys (`prior_sell_state` in `src/live.rs`): hold
  while `Unknown`, release on `Landed` (warn: possibly stale ledger) or
  `Dead`. The stale "Unknown proceeds" comment in `live_mirror.rs` was
  fixed — do not revert it. Engine-`Invalid` (5-minute lookback forget)
  is NOT proof of no-fill; unverifiable reconciles hold (fail closed).
  The TTL release (`SELL_GUARD_TTL_SECS`) is the ONLY evidence-free
  release, deliberately loud; guard checks poll one-shot
  (`SELL_GUARD_RESOLVE_TIMEOUT`) so they never block an exit.
- Pool legs are WSOL-side-first NORMALIZED (`MigratedPool` swaps
  PumpSwap vaults; ticker, spike math, and `PoolLeg` docs all assume
  it), but `SellPumpSwapOrder`, the preflight wiring guard, and the
  `sell_v2` ix speak RAW pool orientation (base vault = token
  inventory). Passing a leg straight through refused every live
  PumpSwap dispatch at that guard (order WSOL vault vs pool token
  vault) — found by audit, never by a green sim (probes construct raw
  orders directly and bypass the leg). The single mapping lives in
  `PoolLeg::pumpswap_sell_order` (unit-pinned), used by
  `LiveTrader::sell_pumpswap`; Raydium needs no mapping (raw ==
  normalized there). Never route around the mapper.
- Early-live dispatch scope is SOL-paired quotes only (both curve
  preflights refuse non-WSOL quotes fail-closed; PumpSwap/Raydium were
  already WSOL-only by their trust gates). The USDC/pump-coin quote
  paths exist in code but never validated live — quote math, ATA
  funding, fee legs, recovery are proven on SOL pairs only. Mayhem
  rides SOL pairs and is unaffected (the gate keys on the quote mint,
  never the curve kind; pinned by `mayhem_sol_quote_passes_scope_gate`).
  Paper is unscoped by design (research freedom); a paper-open on a
  token pair surfaces as diverged. Lift per-quote only after
  quote/ATA/fee/recovery validation for that quote.
- Dispatch status semantics live in `crates/executor/src/executor.rs`
  (`ConfirmOutcome`, `DispatchVerdict`, pinned by `timeout_is_never_a_failure`
  / `chain_verdicts_open_only_on_voted_states`): receipt = submitted
  (acceptance != landing); `Timeout` = unseen, never failure;
  `Processed` is seen-but-unvoted (no position opens on it).
  `resolve_dispatch` composes chain + engine + `reconcile_signature`
  (full history) + blockhash expiry; `Dead` needs proof (on-chain error,
  engine never-forwarded, or unseen + dead blockhash). Never let a
  timeout map back to "failure" or skip the reconcile before a resend.
  Expiry (`unseen_expiry`) is checked ONLY for an unseen signature and
  only an explicit invalid answer counts: a missing/unparsable hash or
  any RPC doubt stays `Unknown` (never dead). Engine `Landed`/`Failed`
  need no reconcile (the confirm poll already queried the chain for
  that signature); `reconcile_signature` is only called when the engine
  has no testimony (`Invalid`/`Unknown`).
- `venue_fee=None` at confirm fails closed (non-synthetic). Test stubs
  default to `Some(125.0)`; the `None`-fee test arm asserts zero opens.

## Hard-earned mainnet facts (don't re-derive)

- New launches are `create_v2` (disc `[214,144,…]`) in **both v0 and v1
  envelopes** (2 v0 + 1 v1 among recent launches; frozen proofs:
  `create_v2_v0_recent_mainnet.json` + `create_v2_v1_recent_mainnet.json`):
  fetch configs must keep `maxSupportedTransactionVersion: 1`, never
  narrow to v1-only. v0 txs use address lookup tables — key-table code
  must include `loadedAddresses`.
- Dispatch is v3-only (`buy_v3`/`sell_v3`, 17 accounts, fees stay on the
  curve). The legacy 12-account core, 18-account mayhem buy, and
  27/26-account `buy_v2`/`sell_v2` shapes are RETIRED from dispatch
  (deleted, Oct-2026): legacy fills are extinct on mainnet and 18/v2
  fills occur only on old curves, while every new launch is a v3 curve.
  Non-v3 curves fail closed (diverged/manual, no funds move). The
  listener still parses all historical shapes (screening + quorum need
  them); only dispatch builders are gone. Do NOT re-add a retired shape
  without a frozen v3-era sample proving a NEW launch uses it.
- Token program is per-mint (classic SPL vs Token-2022); ATA derivation
  must use it. Ticks price with real mint decimals (cached); the confirm
  gate still drops non-6 mints fail-closed. `live::six_decimals` is the
  one decimals admission read (gate drop signal + live buy/sell paths).
- Raydium migration pools are WSOL-**based** (WSOL at slot 400, token at
  432; vault contents prove it) — the old code assumed WSOL-quoted and
  rejected every real pool at both the trust gate and the vault pricer.
  Frozen proofs: `crates/migration/tests/fixtures/raydium_pool_fartcoin.json`
  (canonical Oct-2024 Fartcoin pool) + `raydium_pool_mainnet.json`.
- v2 launches have TWO insider identities (payer `user` + tail `creator`):
  both are screened (worst trust wins) and both join the gate cohort
  (either dump aborts). Prior-launch counting uses the versioned author
  index (v1: 7, v2: 5) — a shared index undercounted v2 serial launchers.
- Sizing takes per-mint `decimals` explicitly (no hidden constant in the
  calc path): the confirm gate verifies decimals per mint and drops
  non-6 fail-closed, and live re-verifies at dispatch. Live dispatch
  preflight is warn-and-proceed unless `SIM_STRICT=true` (strict refuses
  unproven); `SwapExecutor::new` defaults strict ON, paper paths opt
  out explicitly via `with_strict_sim(false)`.
- Fee payer ≠ `user` happens (relayers); holders may sit in aux
  accounts, not the canonical ATA (a live `sell_v2` probe hit exactly
  this: one seller's `[14]` was aux, so the builder's canonical-ATA
  assumption fails closed there — our own holdings are always canonical
  via the idempotent ATA prefix). Simulation with `sig_verify:false`
  proves layout/logic, never authorization.
- Dispatch is Jito-bundles-only (`send_bundle`): no public-mempool
  send exists anywhere (deleted — sniper flow must never face
  sandwich exposure). Do NOT re-add one without a written MEV analysis.
- PumpSwap `sell_v2` IS sim-proven now (`pumpswap_sell_v2_simulates`:
  builder + quote floor vs live pool + live funded holder, GREEN) and
  byte-pinned by a frozen fill (`sell_v2_pumpswap_mainnet.json`, slot
  457835709, Token-2022 base — `sell_pumpswap_v2_matches_frozen_fill`).
  Still no funded trial (buy-then-sell for real) — that trial is an
  operator action, never run from here.
- Mainnet fixtures live in `crates/listener/tests/fixtures/`; replay
  tests are the gate that must stay green.

## Risk-engine inference (don't re-derive)

- JSON-mode 400 `json_validate_failed` with empty `failed_generation` =
  reasoning tokens ate the completion budget (live gpt-oss-120b: reasoning
  alone ran 561–687 tokens, completion 603–720, for a ~35-token JSON
  answer). `DEFAULT_MAX_TOKENS=1024` / `MAX_MAX_TOKENS=2048` are sized for
  that — do not lower them; 128 and even 512 fail.
- `SYSTEM_PROMPT` (`crates/risk-engine/src/schema.rs`) must keep the
  "Output schema" sentence naming every `RiskProfile` key verbatim; without
  it live models invent `tp`/`sl`/`trailing`, decode fails, and every
  response fail-closes to PASS (`invalid_count`). Guard test:
  `prompt_names_every_output_field`. Live-proven (curl 2/2 compliant;
  engine 3/4 Model, rest timeout→fallback).
- Request shape stays provider-neutral OpenAI-compatible (`max_tokens` +
  `response_format {"type":"json_object"}`); no provider-specific fields,
  no Groq support (removed — don't re-add). Undecodable output → PASS;
  timeout → heuristic fallback (`FALLBACK_TIMEOUT` 300ms default,
  `MAX_TIMEOUT` 2s clamp). The model is a second screener, never a gate
  override: `confirm_metrics` (`src/gate.rs`, pure, unit-pinned) feeds it
  the structured tape (`MicroSignals` + `top_holder_pct` + volume) and
  the profile flows through the same `open_tracked` choke as every
  refusal — a model BUY cannot pass a tripped breaker, cap, or pause.
  Prompt rules only tighten (PASS/temper on wash, concentration,
  insider distribution, fading velocity); the heuristic keeps the
  velocity screen and ignores the newer context fields.
- Explicit fallback policy (`inference_fallback`: `heuristic` | `pass`,
  default heuristic): on any inference failure the operator chooses
  validated bands or skip — never a hotter fallback to protect volume.
  Single choke in confirm (engine-absent and engine-failed both arrive
  as `Fallback`). Confidence telemetry per model: latency p50/p95/p99
  over a bounded window (timeouts sit at the budget they cost) plus
  fallback BUY vs PASS counts, on the heartbeat — a volume-protecting
  fallback can't hide behind the rate alone.
- Exit regimes are a closed set (`ExitRegime`: conservative/momentum/
  liquidity_sensitive): the model picks, deterministic `resolve_regime`
  (`crates/risk-engine/src/strategy.rs`) snaps the numbers to the
  regime's tested profile — free-form TP/SL never reach the book, PASS
  and invalid pass through, and the heuristic's band profiles already
  equal the conservative/momentum consts (snap idempotent).
  `force_exit_regime` (`""` = LLM selects) is the fixed baseline for
  evaluation and the LLM kill path; replay bins expectancy per recorded
  regime in `Open.regime` so selection is judged against the baseline.
- Research agent (`pump-sentry research`, `src/research.rs`): offline
  loss-pattern analysis over the journal — loss clusters plus ranked
  filter proposals with avoided-vs-foregone math and halves. READ-ONLY
  by construction (no write path to config/wallet/dispatch); promoting
  anything requires operator config edit, paper backtest, live confirm,
  explicit approval. Paper patterns are never live evidence.
- Measured latency per decision: gpt-oss-120b ~1.7–2.1s (mostly
  timeout→fallback at the 2s clamp), qwen3.8-27b ~0.3s (fits). Remote
  gateways need `inference_timeout_ms` raised (README `[risk]` example).

## Toolchain quirks

- Mixed Solana versions by design: `executor` builds on granular
  v3/v4 crates (`solana-client 4.3`), not `solana-sdk 5`.
  Don't "unify" them. Never copy lockfile-resolved versions back into
  `Cargo.toml` (it flipped `curve`/`migration` to `solana-account 5.x`
  against `client 4.3`'s 4.x returns and broke the build): manifest
  majors are architecture, `cargo update` touches the lockfile only.
- Rust 1.99 (`rust-version` in manifests). Let-chains (`if let … && let`)
  are used and required.
- Long RPC-bound commands: run in background, don't poll; prefer bulk
  `getBlock` scans over per-signature loops (rate limits).
- Geyser 429s trip a circuit breaker at 30 consecutive (~30 min):
  retries drop to hourly, a Discord card fires, the heartbeat re-logs
  while banned. Never "fix" the hourly cadence or delete the breaker —
  hammering a ban sustains it. Recovery is automatic on a working
  subscribe; the operator procedure lives in `ops/RUNBOOK.md` §7.
- Commits in this repo are authored as the project owner; if `git commit`
  fails on gpg signing, retry with `-c commit.gpgsign=false`. Never push
  unless asked.

## Working conventions

- Prefer `edit` over new files; derive `oldString` from current content.
- Reference code as `path:line`. Keep responses short, factual, no
  superlatives or emojis. User communicates in Indonesian — answer in
  Indonesian.
- Report honestly: dead ends, failed probes, and rejected hypotheses are
  results. Never fabricate on-chain layouts — mark unverified parts and
  gate them with failing tests.
- Version bumps move in lockstep: root `Cargo.toml` + all 9
  `crates/*/Cargo.toml` + the `ops/ROADMAP.md` heading ("post-X.Y.Z");
  `Cargo.lock` follows automatically (path deps carry no version reqs).
  Commit style: `chore(release): bump to X.Y.Z`.

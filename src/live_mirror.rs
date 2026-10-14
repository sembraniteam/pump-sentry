//! Real-funds mirror: live dispatch, rechecks, sells, manual exits.
//!
//! Moved verbatim from `pipeline.rs` (same `impl Pipeline` block, new
//! home). The paper ledger never touches these paths except through
//! `on_tick` / `manual_sell` / open dispatch. Behavior unchanged.

use std::collections::HashMap;
use std::sync::Arc;

use alert::Alerter;
use curve::CurveError;
use executor::ExecutorError;
use simulator::ExitReason;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::live::{
    LiveBuyParams, LiveError, LivePosition, LiveTrader, RecheckOutcome,
    SELL_MAX_ATTEMPTS, SELL_RETRY_COOLDOWN,
};
use crate::pipeline::{
    AlertSink, Auditor, LiveSink, Pipeline, ReserveFeed, Screener, CURVE_TICK_STALE_SECS,
};
use crate::settings::now_secs;

/// Base mint behind a live-tracking key. Parked duplicate bags carry a
/// `#`-suffixed key (`MINT#2-<bundle>`, `MINT#late-<bundle>`, WAL replays)
/// so they never clobber the first bag; Base58 never contains `#`, so the
/// prefix is always the mint. Heat and duplicate guards must compare on
/// this, never on raw keys — otherwise a mint whose only bag is parked
/// reads as "not exposed" and a second fill dispatches over real custody.
pub fn live_base_mint(key: &str) -> &str {
    key.split('#').next().unwrap_or(key)
}

/// Short tag for suffixed live-tracking keys (`MINT#2-<tag>`). Byte
/// slicing would panic on a non-ASCII boundary; bundle UUIDs and
/// signatures are ASCII today, but keys must never depend on that.
fn short_tag(s: &str) -> String {
    s.chars().take(8).collect()
}
/// Real-funds execution state, split out of [`Pipeline`] so paper-only
/// code paths never touch live bookkeeping. Owns the only live-typed
/// generic (`L`); everything else on `Pipeline` is funds-free.
pub struct LiveExecution<L = LiveTrader> {
    /// Live dispatcher; `None` keeps the run paper-only.
    pub live: Option<L>,
    /// Tracked live fills by mint.
    pub live_positions: Mutex<HashMap<String, LivePosition>>,
    /// Live-sell retry bookkeeping: `(attempts, last_attempt_secs)`.
    pub live_retry: Mutex<HashMap<String, (u32, u64)>>,
    /// Mints that gave up the automated exit: the position is still held
    /// on-chain (never silently dropped) but needs a human exit — kept
    /// here (not in `live_positions`) so the tick loop stops probing
    /// while the shutdown snapshot still reports them. Entries leave
    /// only through an explicit `sell <mint>` (resurrected for the exit)
    /// or not at all: time/count eviction would orphan a real bag the
    /// next time the mint is bought, so growth here is bounded by
    /// operator review in practice, not by code.
    pub live_manual: Mutex<HashMap<String, LivePosition>>,
    /// Paper-open but live-missing: the live buy definitively failed
    /// (bad order / dispatch error, no fill, no pending) while paper
    /// stayed open. No funds at risk, but paper PnL is live-fake — kept
    /// here so the shutdown snapshot and heat cap see the divergence
    /// instead of silently trusting paper. Unknown fills (no signature)
    /// park in `live_manual` instead (may hold, needs exit review).
    pub live_diverged: Mutex<HashMap<String, String>>,
    /// Mints opened on the manual `QUOTE_USD` override (paper-only by
    /// construction). Their paper PnL is priced from an operator-set
    /// number, so it must not feed the day-loss breaker (fiction would
    /// trip — or mask — the gate). Cleared with `live_diverged` when no
    /// paper position remains.
    pub synthetic_mints: Mutex<std::collections::HashSet<String>>,
    /// Paper PnL closed while the live leg still holds: recording it
    /// immediately would let paper profit mask real live loss at the
    /// day-loss gate, but dropping it (as before) undercounts the loss
    /// forever. Stashed here on defer, flushed into the breaker when the
    /// live leg settles (sold / manual-parked / gone). Bounded O(1) per
    /// mint: a running `(total, count)` (sums are what the breaker
    /// records, so aggregation is exact), removed on flush and on
    /// book-empty cleanup.
    pub deferred_pnl: Mutex<HashMap<String, (f64, usize)>>,
    /// Per-mint dispatch locks: probe sizing and tracking updates must
    /// not interleave across concurrent ticks for one mint, but a slow
    /// confirm on mint A must never block exits on mint B (a global
    /// lock did exactly that in volatile markets). One small mutex per
    /// live-traded mint, evicted by `maybe_unwatch` on a confirmed-empty
    /// book (clones held by in-flight tasks keep working after eviction,
    /// so eviction is safe and the map stays bounded on long runs).
    pub live_sell_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl<L> LiveExecution<L> {
    /// Empty execution state (no dispatcher: paper-only).
    pub fn paper() -> Self {
        Self {
            live: None,
            live_positions: Mutex::new(HashMap::new()),
            live_retry: Mutex::new(HashMap::new()),
            live_manual: Mutex::new(HashMap::new()),
            live_diverged: Mutex::new(HashMap::new()),
            synthetic_mints: Mutex::new(std::collections::HashSet::new()),
            deferred_pnl: Mutex::new(HashMap::new()),
            live_sell_locks: Mutex::new(HashMap::new()),
        }
    }

    /// Attach a dispatcher (`None` keeps the paper-only state).
    pub fn with_live(mut self, live: Option<L>) -> Self {
        self.live = live;
        self
    }

    // -- Single-discipline custody API --------------------------------
    //
    // Every production touch of the live book goes through these
    // methods (one lock sequence each, documented here) instead of
    // locking the maps ad hoc at call sites: the lifecycle (tracked →
    // retry → manual/diverged → settled) stays greppable in one place,
    // and a future storage change touches these alone. The maps stay
    // `pub` for tests, which assert exact book contents.

    /// True when a tracked fill exists under the exact mint key.
    pub async fn is_tracked(&self, mint: &str) -> bool {
        self.live_positions.lock().await.contains_key(mint)
    }

    /// Tracked fill clone, if any.
    pub async fn tracked(&self, mint: &str) -> Option<LivePosition> {
        self.live_positions.lock().await.get(mint).cloned()
    }

    /// Insert (or replace) the tracked fill.
    pub async fn insert_tracked(&self, position: LivePosition) {
        self.live_positions
            .lock()
            .await
            .insert(position.mint.clone(), position);
    }

    /// Remove and return the tracked fill, if any.
    pub async fn remove_tracked(&self, mint: &str) -> Option<LivePosition> {
        self.live_positions.lock().await.remove(mint)
    }

    /// Subtract a confirmed sell from the tracked fill; returns true
    /// when the bag emptied out (and was removed).
    pub async fn subtract_fill(&self, mint: &str, amount: u64) -> bool {
        let mut guard = self.live_positions.lock().await;
        match guard.get_mut(mint) {
            Some(tracked) => {
                tracked.amount_tokens = tracked.amount_tokens.saturating_sub(amount);
                if tracked.amount_tokens == 0 {
                    guard.remove(mint);
                    true
                } else {
                    false
                }
            }
            None => true,
        }
    }

    /// True when a manual-parked bag exists under the exact mint key.
    /// (Suffixed duplicates park under `MINT#…` — base-mint matching
    /// lives in `is_live_tracked`, which sees every bag.)
    pub async fn manual_holds(&self, mint: &str) -> bool {
        self.live_manual.lock().await.contains_key(mint)
    }

    /// Park a bag for human exit under an explicit key (exact mint for
    /// give-ups, suffixed for duplicate fills that must not clobber).
    pub async fn park_manual(&self, key: String, position: LivePosition) {
        self.live_manual.lock().await.insert(key, position);
    }

    /// Remove and return the exact-key manual bag, if any.
    pub async fn take_manual_exact(&self, mint: &str) -> Option<LivePosition> {
        self.live_manual.lock().await.remove(mint)
    }

    /// Curve of the exact-key manual bag, if any (resurrect lookup).
    pub async fn manual_curve(&self, mint: &str) -> Option<String> {
        self.live_manual
            .lock()
            .await
            .get(mint)
            .map(|held| held.curve.clone())
    }

    /// True when any manual-parked bag belongs to the base mint
    /// (exact or suffixed key). Used by heat/duplicate guards so a
    /// parked duplicate never reads as "not exposed".
    pub async fn manual_holds_base(&self, mint: &str) -> bool {
        self.live_manual
            .lock()
            .await
            .keys()
            .any(|k| live_base_mint(k) == mint)
    }

    /// All tracked fills (exact mint keys) for reporting.
    pub async fn tracked_keys(&self) -> Vec<String> {
        self.live_positions.lock().await.keys().cloned().collect()
    }

    /// All manual keys (exact + suffixed) for reporting.
    pub async fn manual_keys(&self) -> Vec<String> {
        self.live_manual.lock().await.keys().cloned().collect()
    }

    /// Mark paper-open but live-missing (definitive no-fill).
    pub async fn mark_diverged(&self, mint: &str, reason: String) {
        self.live_diverged
            .lock()
            .await
            .insert(mint.to_string(), reason);
    }

    /// Clear the diverged mark (paper lifecycle ended or fill healed it).
    pub async fn clear_diverged(&self, mint: &str) {
        self.live_diverged.lock().await.remove(mint);
    }

    /// All diverged mints for reporting.
    pub async fn diverged_keys(&self) -> Vec<String> {
        self.live_diverged.lock().await.keys().cloned().collect()
    }

    /// Record one failed sell probe: bump attempts, stamp the cooldown,
    /// return the new attempt count.
    pub async fn note_failure(&self, mint: &str, now: u64) -> u32 {
        let mut retry = self.live_retry.lock().await;
        let entry = retry.entry(mint.to_string()).or_insert((0u32, 0u64));
        entry.0 = entry.0.saturating_add(1);
        entry.1 = now;
        entry.0
    }

    /// Stamp the sell-retry cooldown without consuming the attempt
    /// budget (dust re-probes: unsellable, but not failures).
    pub async fn stamp_cooldown(&self, mint: &str, now: u64) {
        self.live_retry
            .lock()
            .await
            .entry(mint.to_string())
            .or_insert((0u32, 0u64))
            .1 = now;
    }

    /// Drop the sell-retry state (success, empty book, or parked).
    pub async fn reset_retry(&self, mint: &str) {
        self.live_retry.lock().await.remove(mint);
    }

    /// True when the last failure is still inside the cooldown.
    pub async fn cooling(&self, mint: &str, now: u64, cooldown_secs: u64) -> bool {
        self.live_retry.lock().await.get(mint).is_some_and(
            |(_, last)| now.saturating_sub(*last) < cooldown_secs,
        )
    }

    /// Mark a mint synthetic (override-priced paper: never feeds the
    /// day-loss breaker).
    pub async fn mark_synthetic(&self, mint: &str) {
        self.synthetic_mints.lock().await.insert(mint.to_string());
    }

    /// True when the mint is synthetic.
    pub async fn is_synthetic(&self, mint: &str) -> bool {
        self.synthetic_mints.lock().await.contains(mint)
    }

    /// Clear the synthetic mark (paper lifecycle ended).
    pub async fn clear_synthetic(&self, mint: &str) {
        self.synthetic_mints.lock().await.remove(mint);
    }

    /// Remove and return the stashed deferred total, if any.
    pub async fn take_deferred(&self, mint: &str) -> Option<(f64, usize)> {
        self.deferred_pnl.lock().await.remove(mint)
    }

    /// Stash closed paper PnL for the breaker while the live leg still
    /// holds (flushed on settle). Mirrors `DailyBreaker::record`:
    /// non-finite legs never feed the gate.
    pub async fn stash_deferred_pnl(&self, mint: &str, pnls: Vec<f64>) {
        if pnls.is_empty() {
            return;
        }
        let mut stashed = self.deferred_pnl.lock().await;
        let slot = stashed.entry(mint.to_string()).or_insert((0.0, 0));
        for pnl in pnls {
            if pnl.is_finite() {
                slot.0 += pnl;
                slot.1 += 1;
            }
        }
    }

    /// Distinct live-exposure mints for the heat cap: tracked, manual
    /// (base-mint stripped, so parked duplicates count), plus pending.
    /// Sorted + deduped for stable logs.
    pub async fn exposure_mints(&self, pending: Vec<String>) -> Vec<String> {
        let mut exposure: Vec<String> =
            self.live_positions.lock().await.keys().cloned().collect();
        exposure.extend(
            self.live_manual
                .lock()
                .await
                .keys()
                .map(|k| live_base_mint(k).to_string()),
        );
        exposure.extend(pending);
        exposure.sort();
        exposure.dedup();
        exposure
    }
}
impl<A: Auditor, R: ReserveFeed, L: LiveSink, S: Screener, T: AlertSink> Pipeline<A, R, L, S, T> {
    /// True when any live bag exists for `mint`: tracked, parked manual
    /// (exact or `#`-suffixed duplicate keys — compared by base mint),
    /// or pending confirm. Single home for the duplicate/heat guard so a
    /// parked bag can never read as "not exposed".
    pub(crate) async fn is_live_tracked(&self, trader: &L, mint: &str) -> bool {
        if self.exec.is_tracked(mint).await {
            return true;
        }
        if self.exec.manual_holds_base(mint).await {
            return true;
        }
        trader.pending_mints().iter().any(|m| m == mint)
    }

    /// Mirror a paper open with a real Jito-routed buy (live mode only).
    /// Paper opens first; live failures park explicitly (diverged for
    /// definitive no-fill, manual for unknown fills) so paper never
    /// silently poses as live. Fail-closed on stale SOL/USD.
    /// The SOL reading is shared with entry pricing (captured once at
    /// confirm in [`TrackedOpen::sol`]) — amount and guard price one
    /// market. Freshness is re-checked HERE at use time (`age_ok`): a
    /// reading aged past the feed window by dispatch fails closed, and
    /// any residual intra-tick drift dies at simulation (6002), never
    /// as an unprotected dispatch.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn live_buy(
        &self,
        mint: &str,
        curve: &str,
        assoc_curve: &str,
        entry_quote: f64,
        liquidity_usd: f64,
        sol: Option<(f64, u64)>,
        creator: Option<&str>,
    ) {
        let Some(trader) = &self.exec.live else { return };
        let Some((sol_usd, _ts)) = sol.filter(|(_, ts)| self.sol_feed.age_ok(*ts)) else {
            warn!(%mint, "stale SOL/USD — live buy skipped (no static fallback)");
            // Buys are one-shot at open (never retried), so a skip leaves
            // paper permanently uncovered — mark it diverged like every
            // other skip path. Healed by a later successful buy; cleared
            // when the paper position closes (see `on_tick`).
            self.exec
                .mark_diverged(mint, "live buy skipped: stale SOL/USD".to_string())
                .await;
            self.emit(format!("LIVE buy skipped on {mint} (stale SOL/USD); paper open WITHOUT live cover"));
            return;
        };
        // Per-mint serialization (same lock map as `live_sell`): two
        // concurrent buys for one mint must not both dispatch (the
        // second fill would orphan), and a buy racing a sell for the
        // same mint must not interleave sizing with custody updates.
        // Other mints proceed freely.
        let serial = Self::lock_for(&self.exec.live_sell_locks, mint).await;
        let _serial = serial.lock().await;
        // Live heat: paper heat caps paper slots exactly, but live bags
        // outlive paper (failed sells park to manual/pending while the
        // paper slot frees). Count distinct live-exposure mints so real
        // funds cannot stack past the cap while paper looks empty.
        {
            let exposure = self.exec.exposure_mints(trader.pending_mints()).await;
            let already_exposed = exposure.iter().any(|m| m == mint);
            if !already_exposed && exposure.len() >= self.settings.max_open_positions {
                warn!(%mint, cap = self.settings.max_open_positions, "live heat limit: paper opens, live mirror skipped (real exposure at cap)");
                self.exec
                    .mark_diverged(mint, "live buy skipped: heat limit".to_string())
                    .await;
                self.emit(format!("live heat limit on {mint}: paper-only, live skipped (exposure at cap)"));
                return;
            }
        }
        // Pre-dispatch duplicate guard: never spend a bundle on a mint
        // already tracked / manual / pending. The post-dispatch check
        // below stays as defense-in-depth (a concurrent buy may have
        // landed first while this one was in flight).
        if self.is_live_tracked(trader, mint).await {
            warn!(%mint, "LIVE buy skipped: mint already tracked/manual/pending; no dispatch (paper stays open, no new funds)");
            self.emit(format!("LIVE buy skipped on already-tracked {mint}; no dispatch"));
            return;
        }
        match trader
            .buy(&LiveBuyParams {
                mint,
                curve,
                assoc_curve,
                notional_usd: self.settings.notional_usd,
                price_usd: entry_quote,
                sol_usd,
                liquidity_usd,
                // Same tier the paper ledger registered at open: sizing
                // and guard match the booked fills exactly.
                venue_fee_bps: Some(self.portfolio.lock().await.venue_fee_for(mint)),
                creator,
            })
            .await
        {
            Ok((position, bundle)) => {
                // Defense-in-depth: the pre-dispatch guard above passed,
                // but a concurrent buy may have landed first while this
                // dispatch was in flight. The second fill is REAL on-chain
                // custody — park it in `live_manual` (human exit), never
                // drop it silently.
                if self.is_live_tracked(trader, mint).await {
                    warn!(%mint, %bundle, amount = position.amount_tokens, "LIVE buy landed for already-tracked mint; parked as manual (still HELD), MANUAL REVIEW REQUIRED");
                    trader.record_manual(&position, &bundle);
                    self.exec
                        .park_manual(format!("{mint}#2-{}", short_tag(&bundle)), position)
                        .await;
                    self.emit(format!("second LIVE bag on {mint} ({bundle}); MANUAL EXIT REQUIRED"));
                    return;
                }
                let amount = position.amount_tokens;
                // WAL first (write-ahead): a crash between here and the
                // insert below replays to a tracked fill, never to loss.
                trader.acknowledge_buy(&position, &bundle);
                self.exec.insert_tracked(position).await;
                // A belated live fill heals a prior diverged mark for the
                // same mint (paper is live-backed again).
                self.exec.clear_diverged(mint).await;
                // Real dispatch cost hits the day-loss gate now (tip +
                // priority fee + base fee left the wallet whether or not
                // the trade wins, plus one ATA rent reserve: this is the
                // first touch of a new mint).
                let buy_tip_usd =
                    trader.first_touch_cost_lamports() as f64 / 1_000_000_000.0 * sol_usd;
                if buy_tip_usd.is_finite() && buy_tip_usd > 0.0 {
                    self.daily.lock().await.record(-buy_tip_usd, now_secs());
                    self.portfolio.lock().await.debit_dispatch_cost(buy_tip_usd);
                }
                warn!(%mint, "LIVE buy dispatched via Jito (real funds)");
                self.emit(Alerter::format_live_buy(mint, &bundle, amount));
            }
            Err(LiveError::UnknownDispatch { position, reason }) => {
                // No signature, no recheck possible: the fill may or may
                // not exist. Park the intended bag as manual (possibly
                // holding — needs an exit review), never drop it.
                warn!(%mint, error = %reason, "LIVE buy dispatch unknown; parked for manual review (may hold — check mint activity on explorer, no signature exists)");
                trader.record_manual(&position, "dispatch-unknown");
                self.exec.park_manual(mint.to_string(), *position).await;
                self.emit(format!("LIVE buy dispatch unknown on {mint}; MANUAL REVIEW REQUIRED (fill unknown)"));
            }
            Err(LiveError::Unconfirmed(msg))
                if msg.contains("Confirmed failure:") =>
            {
                // Proven on-chain failure fast path (bundle errored,
                // never a fill): the pending entry is already untracked
                // inside the trader. No funds at risk — but paper is now
                // live-fake, so mark it diverged like any other no-fill
                // instead of pretending it is still pending recheck.
                warn!(%mint, error = %msg, "LIVE buy CONFIRMED-FAILED on-chain (no fill); paper still open but DIVERGED");
                self.exec.mark_diverged(mint, msg).await;
                self.emit(format!("LIVE buy confirmed-failed on {mint}; paper open WITHOUT live cover"));
            }
            Err(LiveError::Unconfirmed(msg)) => {
                // Pending is already tracked inside the trader for the
                // receipt-bearing path: the recheck loop owns it. Warn
                // only; GiveUp later parks it to manual (never silent).
                warn!(%mint, error = %msg, "LIVE buy unconfirmed; tracked as pending, rechecking");
            }
            Err(e) => {
                // Definitive no-fill (bad order / dispatch reject): paper
                // stays open but is explicitly diverged — no funds at
                // risk, paper PnL must not be read as live.
                warn!(%mint, error = %e, "LIVE buy FAILED (no fill); paper position still open but DIVERGED");
                self.exec.mark_diverged(mint, e.to_string()).await;
                self.emit(format!("LIVE buy failed on {mint} ({e}); paper open WITHOUT live cover"));
            }
        }
    }

    /// Re-poll unconfirmed live buys: promote landed fills to tracked
    /// (with exit automation), drop on-chain failures, escalate give-ups
    /// to manual review. No-op when nothing is pending.
    pub(crate) async fn recheck_live_pending(&self) {
        let Some(trader) = &self.exec.live else { return };
        if !trader.has_pending() {
            return;
        }
        for recheck in trader.recheck_pending().await {
            let mint = recheck.position.mint.clone();
            match recheck.outcome {
                RecheckOutcome::Confirmed => {
                    // Same no-overwrite rule as the fast buy path: a
                    // late confirmation never clobbers a newer bag. The
                    // late fill is still real custody — park it manual.
                    if self.is_live_tracked(trader, &mint).await {
                        warn!(mint = %mint, bundle = %recheck.bundle_id, amount = recheck.position.amount_tokens, "late LIVE buy confirm for already-tracked mint; parked as manual (still HELD), MANUAL REVIEW REQUIRED");
                        trader.record_manual(&recheck.position, &recheck.bundle_id);
                        self.exec
                            .park_manual(
                                format!("{mint}#late-{}", short_tag(&recheck.bundle_id)),
                                recheck.position,
                            )
                            .await;
                        self.emit(format!("late LIVE buy confirm on already-tracked {mint}; MANUAL REVIEW REQUIRED"));
                        continue;
                    }
                    let amount = recheck.position.amount_tokens;
                    // WAL first (write-ahead), then track.
                    trader.acknowledge_buy(&recheck.position, &recheck.bundle_id);
                    self.exec.insert_tracked(recheck.position).await;
                    self.exec.clear_diverged(&mint).await;
                    // Same first-touch accounting as the fast path: the
                    // dispatch overhead plus one ATA rent reserve left
                    // the wallet even though the fill landed late.
                    // Stale feed → skip (never invent a conversion rate).
                    if let Some(sol_usd) = self.sol_feed.price() {
                        let tip_usd =
                            trader.first_touch_cost_lamports() as f64 / 1_000_000_000.0 * sol_usd;
                        if tip_usd.is_finite() && tip_usd > 0.0 {
                            self.daily.lock().await.record(-tip_usd, now_secs());
                            self.portfolio.lock().await.debit_dispatch_cost(tip_usd);
                        }
                    }
                    warn!(mint = %mint, "LIVE buy confirmed late; now tracked with exits");
                    self.emit(Alerter::format_live_buy(&mint, &recheck.bundle_id, amount));
                }
                RecheckOutcome::FailedOnChain => {
                    warn!(mint = %mint, bundle = %recheck.bundle_id, "pending LIVE buy errored on-chain; no fill, dropping");
                }
                RecheckOutcome::StillUnknown => {
                    warn!(mint = %mint, bundle = %recheck.bundle_id, "pending LIVE buy still unconfirmed; keeping for next round");
                }
                RecheckOutcome::GiveUp => {
                    // Out of rechecks with no verdict: the fill may exist
                    // on-chain. Park the intended bag as manual (possibly
                    // holding) so the shutdown snapshot reports it —
                    // never drop custody silently.
                    warn!(mint = %mint, bundle = %recheck.bundle_id, sig = %recheck.signature, "pending LIVE buy unconfirmed after retries; parked for MANUAL REVIEW (may hold — look up sig on explorer)");
                    trader.record_manual(&recheck.position, &recheck.signature);
                    self.exec.park_manual(mint.clone(), recheck.position).await;
                    self.emit(format!("LIVE buy unconfirmed on {mint} (sig {}); MANUAL REVIEW REQUIRED (may hold)", recheck.signature));
                }
            }
        }
    }

    /// Flush paper PnL deferred while the live leg was holding, now that
    /// it settled. Synthetic (override-priced) PnL is dropped, never
    /// recorded — same exclusion as the fast path. Idempotent: the entry
    /// is removed, so a second flush is a no-op.
    pub(crate) async fn flush_deferred_pnl(&self, mint: &str) {
        let stashed = self.exec.take_deferred(mint).await;
        let Some((total, n)) = stashed else { return };
        if n == 0 {
            return;
        }
        if self.exec.is_synthetic(mint).await {
            warn!(mint = %mint, n, "synthetic deferred PnL excluded from day-loss breaker (override price, no market)");
            return;
        }
        let now = now_secs();
        let mut daily = self.daily.lock().await;
        if total.is_finite() {
            daily.record(total, now);
        }
    }

    /// Mirror paper closes with proportional Jito-routed sells (live mode
    /// only). Fail-closed on stale SOL/USD. Returns true when the live
    /// leg is settled (sold, parked to manual, or nothing held) and the
    /// paper PnL may feed the breaker; false when live still holds and
    /// the breaker must defer (recording paper profit while holding the
    /// bag would make the day-loss gate optimistic vs real loss).
    pub(crate) async fn live_sell(&self, mint: &str, price_usd: f64, liquidity_usd: f64, fraction_closed: f64) -> bool {
        let Some(trader) = &self.exec.live else { return true };
        let Some(sol_usd) = self.sol_feed.price() else {
            // No stamp: the next tick must retry immediately when the
            // feed recovers (a cooldown stamp here would delay exits
            // past recovery by 30s). Loud warn + card instead.
            warn!(%mint, "stale SOL/USD — live sell skipped (no static fallback); will retry next tick");
            self.emit(format!("stale SOL/USD — live sell stalled on {mint}; exits resume when feed recovers"));
            // Still holding iff a live bag exists anywhere (positions,
            // manual, or pending) — else nothing to defer.
            return !self.is_live_tracked(trader, mint).await;
        };
        // Per-mint serialization: sizing and tracking stay atomic across
        // concurrent ticks for THIS mint, while other mints exit freely.
        let serial = Self::lock_for(&self.exec.live_sell_locks, mint).await;
        let _serial = serial.lock().await;
        // Cooldown + attempt budget: never spam bundles at a dead market.
        // Only the last FAILURE is tracked — successes reset the budget
        // (see below), so a TP scale-out never throttles its own runner
        // exit. Decide under one short lock, then release before any
        // await or second lock (no nested acquisition, no lock held
        // across await).
        let now = now_secs();
        let cooling = self
            .exec
            .cooling(mint, now, SELL_RETRY_COOLDOWN.as_secs())
            .await;
        if cooling {
            return false;
        }
        // Floor at the mint's registered tier (paper parity): the ledger
        // booked this tier at open, so the live floor must match it — a
        // 125-priced floor on a 345 coin authorizes an unfillable
        // minimum. Short lock, never held across awaits.
        let venue_fee_bps = self.portfolio.lock().await.venue_fee_for(mint);
        let probe = {
            let Some(tracked) = self.exec.tracked(mint).await else {
                // Nothing held: any earlier deferral settles now (usually
                // empty — the flush is a no-op then).
                self.flush_deferred_pnl(mint).await;
                return true;
            };
            // Integer sell math: (amount * numer)/10000, never f64 truncation
            // past 2^53 — floor, never ceiling (oversell is unauthorized).
            let numer = (fraction_closed.clamp(0.0, 1.0) * 10_000.0).floor() as u64;
            let held = tracked.amount_tokens;
            let sell_amount = ((held as u128 * numer as u128) / 10_000u128) as u64;
            if sell_amount > held {
                warn!(mint = %mint, sell_amount, held, "live sell sizing overflow; refusing");
                return false;
            }
            // Floor (never ceiling): the live leg must never oversell
            // custody — any dust shortfall self-heals via the no-paper
            // retry path, which re-sells the remainder at full fraction.
            // NOT settled for the breaker: the live leg still holds (even
            // dust), so recording paper PnL now would mask real exposure.
            // Stamp the cooldown so re-probes don't spam every tick, and
            // leave attempts uncounted: dust can never sell, so parking
            // it to manual would page a human over ~$0. Guard drops
            // before the retry lock (never nested, never across await).
            if sell_amount == 0 {
                warn!(mint = %mint, fraction_closed, held, "live sell sizes to zero dust; holding, will re-probe");
                self.exec.stamp_cooldown(mint, now).await;
                return false;
            }
            LivePosition {
                mint: tracked.mint.clone(),
                curve: tracked.curve.clone(),
                assoc_curve: tracked.assoc_curve.clone(),
                amount_tokens: sell_amount,
                creator: tracked.creator.clone(),
            }
        };
        // Post-migration routing: a validated pool leg means the curve
        // is gone — the curve leg would refuse pre-dispatch (complete)
        // or fail on a drained curve, so route Raydium directly. No leg
        // (PumpSwap, unresolvable pool, evicted follow) keeps the curve
        // leg, which fails closed to manual on its own. Short lock, no
        // awaits inside.
        let pool = self
            .followed
            .lock()
            .await
            .get(mint)
            .and_then(|state| state.pool.clone());
        let sell_result = match &pool {
            Some(leg) => {
                warn!(%mint, amm = %leg.amm, leg_mint = %leg.mint, "routing post-migration exit via Raydium");
                trader.sell_raydium(&probe, leg).await
            }
            None => {
                trader
                    .sell(&probe, price_usd, sol_usd, liquidity_usd, Some(venue_fee_bps))
                    .await
            }
        };
        match sell_result {
            Ok(bundle) => {
                // WAL first (write-ahead): a crash between here and the
                // subtract below replays to a subtracted position, never
                // to a resurrected bag (which would double-sell).
                trader.record_sell(mint, probe.amount_tokens, &bundle);
                // Confirmed on-chain by LiveTrader: mirror the fill locally.
                self.exec.subtract_fill(mint, probe.amount_tokens).await;
                // Any success resets the failure budget: partial fills and
                // later scale-outs are new attempts, never continuations
                // of old failures (previously even successes consumed the
                // 5-attempt budget and stamped the cooldown, throttling
                // the runner exit after a TP scale-out).
                self.exec.reset_retry(mint).await;
                // The corresponding paper PnL was deferred while holding;
                // the fill settles it now.
                self.flush_deferred_pnl(mint).await;
                // Post-spike follow reap: a follow kept past its alert
                // solely for this Raydium exit dies with custody (curve
                // legs never set one — the check is a cheap map read).
                self.maybe_release_follow(mint).await;
                // Real dispatch cost: tip + priority fee + base fee left
                // the wallet, so the day-loss gate sees live bleed, not
                // paper-only profit. Sells never touch ATA rent (the
                // account already exists), so plain dispatch cost here.
                let tip_usd = trader.dispatch_cost_lamports() as f64 / 1_000_000_000.0 * sol_usd;
                if tip_usd.is_finite() && tip_usd > 0.0 {
                    self.daily.lock().await.record(-tip_usd, now_secs());
                    self.portfolio.lock().await.debit_dispatch_cost(tip_usd);
                }
                warn!(%mint, %bundle, "LIVE sell dispatched via Jito (real funds)");
                self.emit(Alerter::format_live_sell(mint, &bundle));
                true
            }
            Err(e) => {
                // Deterministic pre-dispatch rejects (bad keys, dust
                // floor) re-probe at the cooldown cadence — a later tick
                // may price out of dust — but bounded: a permanently
                // unroutable bag parks to manual after the budget instead
                // of holding the feed forever.
                let deterministic = matches!(
                    &e,
                    LiveError::BadOrder(_)
                        | LiveError::Dispatch(ExecutorError::BadOrder(_))
                );
                let attempts = self.exec.note_failure(mint, now).await;
                if attempts >= SELL_MAX_ATTEMPTS {
                    // Give up the AUTOMATED exit, not the position: the
                    // bag is still held on-chain, so park it in
                    // `live_manual` (reported by the shutdown snapshot)
                    // and stop probing — never silently drop custody.
                    self.exec.reset_retry(mint).await;
                    // WAL first (write-ahead): record before removing from
                    // `live_positions`, so a crash replays to manual, not loss.
                    if let Some(held) = self.exec.tracked(mint).await {
                        trader.record_manual(&held, "sell-retries-exhausted");
                    }
                    if let Some(held) = self.exec.remove_tracked(mint).await {
                        self.exec.park_manual(mint.to_string(), held).await;
                    }
                    if deterministic {
                        warn!(%mint, error = %e, "LIVE sell deterministically rejected after budget; parked for manual exit (still HELD)");
                    } else {
                        warn!(%mint, "LIVE sell gave up after retries; parked for manual exit (still HELD)");
                    }
                    self.emit(format!("LIVE sell gave up on {mint}; MANUAL EXIT REQUIRED (bag still held)"));
                    // Auto-exit is over (human owns it): the paper PnL may
                    // settle — the bag is still reported via live_manual.
                    self.flush_deferred_pnl(mint).await;
                    true
                } else if deterministic {
                    warn!(%mint, error = %e, attempts, "LIVE sell rejected before dispatch; holding, will re-probe");
                    false
                } else {
                    warn!(%mint, error = %e, "LIVE sell unconfirmed; holding, will retry");
                    false
                }
            }
        }
    }

    /// Operator-requested full exit (`sell <mint>`): close every paper
    /// position for the mint at `Manual`, then sell the whole tracked
    /// live bag — including a manual-parked one, which is resurrected
    /// into tracked for this explicit exit (the only way parked bags
    /// leave; automatic legs never touch them). Returns true when
    /// settled (nothing held) so the caller drops the request file;
    /// false keeps it for the next round.
    /// The curve comes from tracked custody (live or manual-parked bags
    /// carry it): a paper-only mint has no real bag to exit, so the file
    /// settles with a loud log while paper automation continues. A mint
    /// with neither settles immediately instead of retrying forever.
    pub async fn manual_sell(&self, mint: &str) -> bool {
        // Malformed request files (hand-crafted, not CLI-validated) die
        // here instead of retrying every round forever.
        if executor::parse_pubkey(mint).is_err() {
            warn!(%mint, "manual sell request for a non-pubkey mint; dropping request");
            return true;
        }
        // Per-mint serialization, same order as `on_tick` (serial first,
        // then `live_sell`'s own lock inside) — never inverted.
        let _serial = self.mint_serial(mint).await;
        let curve = {
            if let Some(tracked) = self.exec.tracked(mint).await {
                Some(tracked.curve)
            } else {
                self.exec.manual_curve(mint).await
            }
        };
        let Some(curve) = curve else {
            let paper_open = !self.portfolio.lock().await.positions_for_mint(mint).is_empty();
            if paper_open {
                warn!(%mint, "manual sell on a paper-only mint: no live bag to exit; paper automation continues");
            } else {
                info!(%mint, "manual sell on an untracked mint; nothing to do");
            }
            // Terminal empty: no tick will ever arrive to trigger
            // `maybe_unwatch`, so evict the serial entry created above —
            // otherwise every stray `sell <unknown>` leaks one map entry.
            // Same for the venue-tier override (no book, no tier).
            self.evict_mint_locks(mint).await;
            self.portfolio.lock().await.clear_venue_fee_for_mint(mint);
            return true;
        };
        // Explicit operator exit also covers manual-parked bags: move the
        // bag back to tracked so `live_sell` below can probe it. This is
        // the lifecycle that bounds `live_manual` growth — parked bags
        // leave only through this command (or a restart replay), never
        // through silent eviction (evicting custody tracking would orphan
        // a real on-chain bag the next time the mint is bought).
        if !self.exec.is_tracked(mint).await
            && let Some(held) = self.exec.take_manual_exact(mint).await
        {
            warn!(%mint, amount = held.amount_tokens, "manual sell resurrects a parked bag for explicit exit");
            self.exec.insert_tracked(held).await;
        }
        let tick = match self.reserves.fetch_tick(mint, &curve).await {
            Ok(tick) => tick,
            Err(CurveError::BadMintKey(_)) | Err(CurveError::BadCurveKey(_)) => {
                warn!(%mint, "manual sell request unroutable; dropping request");
                // Terminal: request dropped, no tick follows — evict the
                // serial entry (same leak as the untracked path above),
                // plus the venue-tier override (no book, no tier).
                self.evict_mint_locks(mint).await;
                self.portfolio.lock().await.clear_venue_fee_for_mint(mint);
                return true;
            }
            Err(e) => {
                warn!(%mint, error = %e, "manual sell price unreadable; keeping request for next round");
                return false;
            }
        };
        if now_secs().saturating_sub(tick.observed_at) >= CURVE_TICK_STALE_SECS {
            warn!(mint = %tick.mint, "stale curve tick — manual sell deferred to next round");
            return false;
        }
        let mut portfolio = self.portfolio.lock().await;
        let ids: Vec<u64> = portfolio.positions_for_mint(&tick.mint);
        let mut closed_pnls: Vec<f64> = Vec::new();
        let mut closed_cards: Vec<(String, f64, f64, f64, bool)> = Vec::new();
        let now = now_secs();
        for id in ids {
            match portfolio.close_position(id, tick.price_usd, tick.liquidity_usd, ExitReason::Manual, tick.observed_at) {
                Ok(trade) => {
                    closed_pnls.push(trade.net_pnl);
                    // Realizable equity at this tick (same rule as the
                    // auto-exit path): the manual card deducts exit
                    // costs for survivors instead of book value.
                    let mut marks: HashMap<String, simulator::Mark> = HashMap::new();
                    marks.insert(
                        tick.mint.clone(),
                        simulator::Mark {
                            price: tick.price_usd,
                            liquidity_usd: tick.liquidity_usd,
                        },
                    );
                    let equity = portfolio.snapshot(&marks).equity;
                    info!(mint = %tick.mint, id, reason = ?trade.reason,
                        net = format!("{:+.2}", trade.net_pnl), "paper position closed (manual sell)");
                    closed_cards.push((
                        format!("{:?}", trade.reason),
                        trade.net_pnl,
                        trade.net_pnl_percent,
                        equity,
                        false,
                    ));
                }
                Err(e) => warn!(mint = %tick.mint, id, error = %e, "manual paper close failed"),
            }
        }
        drop(portfolio);
        for (reason, pnl, pct, equity, moonbag) in closed_cards {
            self.emit(Alerter::format_exit(
                &tick.mint, &reason, pnl, pct, equity, moonbag,
            ));
        }
        // Live leg first, breaker second (same settle/defer rule as
        // `on_tick`; synthetic paper never feeds the gate).
        let live_settled = self.live_sell(&tick.mint, tick.price_usd, tick.liquidity_usd, 1.0).await;
        if live_settled {
            if self.exec.is_synthetic(&tick.mint).await {
                warn!(mint = %tick.mint, "synthetic paper PnL excluded from day-loss breaker (override price, no market)");
            } else {
                for pnl in closed_pnls {
                    self.daily.lock().await.record(pnl, now);
                }
            }
        } else {
            warn!(mint = %tick.mint, "manual sell: live still holds; breaker PnL deferred, request kept");
            self.exec.stash_deferred_pnl(&tick.mint, closed_pnls).await;
            return false;
        }
        self.recheck_live_pending().await;
        if self
            .portfolio
            .lock()
            .await
            .positions_for_mint(&tick.mint)
            .is_empty()
        {
            self.exec.clear_diverged(&tick.mint).await;
            self.exec.clear_synthetic(&tick.mint).await;
            // Deferred paper PnL settles here only when no live exposure
            // remains to settle it later (flushed, not dropped — the
            // economics already happened). A still-held bag keeps its
            // stash for the `live_sell` flush paths.
            let live_held = self.exec.is_tracked(&tick.mint).await
                || self.exec.manual_holds(&tick.mint).await;
            if !live_held {
                self.flush_deferred_pnl(&tick.mint).await;
            }
        }
        self.maybe_unwatch(&tick).await;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::LivePosition;

    fn bag(mint: &str, amount: u64) -> LivePosition {
        LivePosition {
            mint: mint.to_string(),
            curve: "CURVE".to_string(),
            assoc_curve: "ASSOC".to_string(),
            amount_tokens: amount,
            creator: None,
        }
    }

    fn exec() -> LiveExecution<LiveTrader> {
        // LiveTrader needs an executor; use Option-less paper state and
        // exercise only the map discipline (no RPC involved).
        LiveExecution::paper().with_live(None)
    }

    #[tokio::test]
    async fn tracked_lifecycle_insert_subtract_remove() {
        let exec = exec();
        assert!(!exec.is_tracked("MINT").await);
        exec.insert_tracked(bag("MINT", 1_000)).await;
        assert!(exec.is_tracked("MINT").await);
        assert_eq!(exec.tracked("MINT").await.expect("bag").amount_tokens, 1_000);
        // Partial subtract keeps the bag with the remainder.
        assert!(!exec.subtract_fill("MINT", 400).await);
        assert_eq!(exec.tracked("MINT").await.expect("bag").amount_tokens, 600);
        // Exact subtract empties and removes.
        assert!(exec.subtract_fill("MINT", 600).await);
        assert!(!exec.is_tracked("MINT").await);
        // Missing bag reads as empty (never blocks settlement).
        assert!(exec.subtract_fill("MINT", 1).await);
        assert!(exec.remove_tracked("MINT").await.is_none());
    }

    #[tokio::test]
    async fn manual_park_resurrect_keeps_duplicates() {
        let exec = exec();
        exec.park_manual("MINT".to_string(), bag("MINT", 100)).await;
        exec.park_manual("MINT#2-abcdef12".to_string(), bag("MINT", 50)).await;
        assert!(exec.manual_holds("MINT").await);
        assert!(exec.manual_holds_base("MINT").await);
        assert!(!exec.manual_holds("OTHER").await);
        assert_eq!(exec.manual_curve("MINT").await.as_deref(), Some("CURVE"));
        // Exact take resurrects one bag; the suffixed duplicate survives.
        let held = exec.take_manual_exact("MINT").await.expect("bag");
        assert_eq!(held.amount_tokens, 100);
        assert!(!exec.manual_holds("MINT").await);
        assert!(exec.manual_holds_base("MINT").await);
        assert_eq!(exec.manual_keys().await, vec!["MINT#2-abcdef12".to_string()]);
    }

    #[tokio::test]
    async fn diverged_mark_clears() {
        let exec = exec();
        exec.mark_diverged("MINT", "no fill".to_string()).await;
        assert_eq!(exec.diverged_keys().await, vec!["MINT".to_string()]);
        exec.clear_diverged("MINT").await;
        assert!(exec.diverged_keys().await.is_empty());
    }

    #[tokio::test]
    async fn retry_budget_counts_failures_not_cooldowns() {
        let exec = exec();
        assert_eq!(exec.note_failure("MINT", 1_000).await, 1);
        assert_eq!(exec.note_failure("MINT", 1_001).await, 2);
        assert!(exec.cooling("MINT", 1_002, 30).await);
        assert!(!exec.cooling("MINT", 9_999, 30).await);
        // Dust stamp refreshes the cooldown without spending budget.
        exec.stamp_cooldown("MINT", 2_000).await;
        assert_eq!(exec.note_failure("MINT", 2_001).await, 3);
        exec.reset_retry("MINT").await;
        assert!(!exec.cooling("MINT", 2_002, 30).await);
        assert_eq!(exec.note_failure("MINT", 2_002).await, 1);
    }

    #[tokio::test]
    async fn synthetic_mark_lifecycle() {
        let exec = exec();
        assert!(!exec.is_synthetic("MINT").await);
        exec.mark_synthetic("MINT").await;
        assert!(exec.is_synthetic("MINT").await);
        exec.clear_synthetic("MINT").await;
        assert!(!exec.is_synthetic("MINT").await);
    }

    #[tokio::test]
    async fn deferred_stash_aggregates_finite_only() {
        let exec = exec();
        assert!(exec.take_deferred("MINT").await.is_none());
        exec.stash_deferred_pnl("MINT", vec![10.0, -3.0, f64::NAN]).await;
        // NaN legs never feed the gate and must not poison the total.
        assert_eq!(exec.take_deferred("MINT").await, Some((7.0, 2)));
        // Idempotent: second take finds nothing.
        assert!(exec.take_deferred("MINT").await.is_none());
        // Empty stash writes nothing (no entry left behind).
        exec.stash_deferred_pnl("MINT", Vec::new()).await;
        assert!(exec.take_deferred("MINT").await.is_none());
    }

    #[tokio::test]
    async fn exposure_counts_parked_duplicates_once() {
        let exec = exec();
        exec.insert_tracked(bag("B", 10)).await;
        exec.insert_tracked(bag("A", 10)).await;
        exec.park_manual("A#2-abcdef12".to_string(), bag("A", 5)).await;
        exec.park_manual("C#late-12345678".to_string(), bag("C", 5)).await;
        let exposure = exec
            .exposure_mints(vec!["C".to_string(), "D".to_string()])
            .await;
        assert_eq!(exposure, vec!["A", "B", "C", "D"]);
    }

    #[tokio::test]
    async fn tracked_and_manual_key_listings() {
        let exec = exec();
        assert!(exec.tracked_keys().await.is_empty());
        exec.insert_tracked(bag("MINT", 1)).await;
        assert_eq!(exec.tracked_keys().await, vec!["MINT".to_string()]);
        assert!(exec.remove_tracked("MINT").await.is_some());
        assert!(exec.tracked_keys().await.is_empty());
    }
}

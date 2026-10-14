//! Live mirror: dispatch, rechecks, sells, manual exits.

use std::collections::HashMap;
use std::sync::Arc;

use alert::Alerter;
use curve::CurveError;
use executor::ExecutorError;
use simulator::ExitReason;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::live::{
    LiveBuyParams, LiveError, LivePosition, LiveSellParams, LiveTrader, RaydiumSellParams,
    RecheckOutcome, SELL_MAX_ATTEMPTS, SELL_RETRY_COOLDOWN, escalated_tip,
};
use crate::pipeline::{
    AlertSink, Auditor, LiveSink, Pipeline, ReserveFeed, Screener, CURVE_TICK_STALE_SECS,
};
use crate::settings::now_secs;

/// Base mint behind a key. `#`-suffix parks dups; compare on this.
pub fn live_base_mint(key: &str) -> &str {
    key.split('#').next().unwrap_or(key)
}

/// Short tag for suffixed keys.
fn short_tag(s: &str) -> String {
    s.chars().take(8).collect()
}
/// Live state, split from [`Pipeline`]. Owns the live generic (`L`).
pub struct LiveExecution<L = LiveTrader> {
    /// Dispatcher; `None` is paper-only.
    pub live: Option<L>,
    /// Tracked fills by mint.
    pub live_positions: Mutex<HashMap<String, LivePosition>>,
    /// Sell retries: `(attempts, last_secs)`.
    pub live_retry: Mutex<HashMap<String, (u32, u64)>>,
    /// Gave-up bags, still held. Leave only via `sell <mint>`.
    pub live_manual: Mutex<HashMap<String, LivePosition>>,
    /// Paper-open but live-missing (no fill). Kept so heat sees it.
    pub live_diverged: Mutex<HashMap<String, String>>,
    /// Override-priced paper mints (PnL never feeds breaker).
    pub synthetic_mints: Mutex<std::collections::HashSet<String>>,
    /// Closed paper PnL stashed while live holds: `(total, count)`.
    pub deferred_pnl: Mutex<HashMap<String, (f64, usize)>>,
    /// Per-mint dispatch locks: slow mint A never blocks B.
    pub live_sell_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl<L> LiveExecution<L> {
    /// Empty state (paper-only).
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

    /// Attach a dispatcher.
    pub fn with_live(mut self, live: Option<L>) -> Self {
        self.live = live;
        self
    }

    // Custody API: one lock sequence each.

    /// True when a fill exists under the exact key.
    pub async fn is_tracked(&self, mint: &str) -> bool {
        self.live_positions.lock().await.contains_key(mint)
    }

    /// Fill clone, if any.
    pub async fn tracked(&self, mint: &str) -> Option<LivePosition> {
        self.live_positions.lock().await.get(mint).cloned()
    }

    /// Insert (or replace) the fill.
    pub async fn insert_tracked(&self, position: LivePosition) {
        self.live_positions
            .lock()
            .await
            .insert(position.mint.clone(), position);
    }

    /// Remove and return the fill, if any.
    pub async fn remove_tracked(&self, mint: &str) -> Option<LivePosition> {
        self.live_positions.lock().await.remove(mint)
    }

    /// Subtract a confirmed sell; true when the bag emptied.
    /// A missing bag is custody drift (dispatch confirmed what we no
    /// longer track): loud, then treated as settled so the breaker
    /// still resolves instead of deferring forever.
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
            None => {
                warn!(%mint, amount, "sell confirmed for an untracked bag; custody drifted, treating as settled");
                true
            }
        }
    }

    /// True when a manual bag exists under the exact key.
    pub async fn manual_holds(&self, mint: &str) -> bool {
        self.live_manual.lock().await.contains_key(mint)
    }

    /// Park a bag under a key (exact or suffixed dup).
    pub async fn park_manual(&self, key: String, position: LivePosition) {
        self.live_manual.lock().await.insert(key, position);
    }

    /// Remove and return the exact-key bag, if any.
    pub async fn take_manual_exact(&self, mint: &str) -> Option<LivePosition> {
        self.live_manual.lock().await.remove(mint)
    }

    /// Curve of the exact-key bag, if any.
    pub async fn manual_curve(&self, mint: &str) -> Option<String> {
        self.live_manual
            .lock()
            .await
            .get(mint)
            .map(|held| held.curve.clone())
    }

    /// True when any parked bag belongs to the base mint.
    pub async fn manual_holds_base(&self, mint: &str) -> bool {
        self.live_manual
            .lock()
            .await
            .keys()
            .any(|k| live_base_mint(k) == mint)
    }

    /// Tracked fills for reporting.
    pub async fn tracked_keys(&self) -> Vec<String> {
        self.live_positions.lock().await.keys().cloned().collect()
    }

    /// Manual keys (exact plus suffixed).
    pub async fn manual_keys(&self) -> Vec<String> {
        self.live_manual.lock().await.keys().cloned().collect()
    }

    /// Mark paper-open but live-missing.
    pub async fn mark_diverged(&self, mint: &str, reason: String) {
        self.live_diverged
            .lock()
            .await
            .insert(mint.to_string(), reason);
    }

    /// Clear the diverged mark.
    pub async fn clear_diverged(&self, mint: &str) {
        self.live_diverged.lock().await.remove(mint);
    }

    /// Diverged mints for reporting.
    pub async fn diverged_keys(&self) -> Vec<String> {
        self.live_diverged.lock().await.keys().cloned().collect()
    }

    /// Record one failed sell; return new count.
    pub async fn note_failure(&self, mint: &str, now: u64) -> u32 {
        let mut retry = self.live_retry.lock().await;
        let entry = retry.entry(mint.to_string()).or_insert((0u32, 0u64));
        entry.0 = entry.0.saturating_add(1);
        entry.1 = now;
        entry.0
    }

    /// Drop retry state (success, empty, or parked).
    pub async fn reset_retry(&self, mint: &str) {
        self.live_retry.lock().await.remove(mint);
    }

    /// True when the last failure is still cooling.
    pub async fn cooling(&self, mint: &str, now: u64, cooldown_secs: u64) -> bool {
        self.live_retry.lock().await.get(mint).is_some_and(
            |(_, last)| now.saturating_sub(*last) < cooldown_secs,
        )
    }

    /// Prior sell failures (tip escalation input; 0 when clean).
    pub async fn retry_attempts(&self, mint: &str) -> u32 {
        self.live_retry.lock().await.get(mint).map(|(n, _)| *n).unwrap_or(0)
    }

    /// Mark a mint synthetic (override paper).
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

    /// Remove and return the stashed total, if any.
    pub async fn take_deferred(&self, mint: &str) -> Option<(f64, usize)> {
        self.deferred_pnl.lock().await.remove(mint)
    }

    /// Stash closed paper PnL while live holds. Skips non-finite legs.
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

    /// Distinct live mints: tracked plus manual (base) plus pending.
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
    /// True when any live bag exists for `mint`.
    pub(crate) async fn is_live_tracked(&self, trader: &L, mint: &str) -> bool {
        if self.exec.is_tracked(mint).await {
            return true;
        }
        if self.exec.manual_holds_base(mint).await {
            return true;
        }
        trader.pending_mints().iter().any(|m| m == mint)
    }

    /// Mirror a paper open with a real buy. Fails closed on stale SOL.
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
            // One-shot buy: a skip leaves paper uncovered.
            self.exec
                .mark_diverged(mint, "live buy skipped: stale SOL/USD".to_string())
                .await;
            self.emit(format!("LIVE buy skipped on {mint} (stale SOL/USD); paper open WITHOUT live cover"));
            return;
        };
        // Per-mint lock: one mint never blocks others.
        let serial = Self::lock_for(&self.exec.live_sell_locks, mint).await;
        let _serial = serial.lock().await;
        // Heat caps live exposure (bags outlive paper).
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
        // Pre-dispatch dup guard (post check stays as backup).
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
                // Same tier as paper: sizing matches booked fills.
                venue_fee_bps: Some(self.portfolio.lock().await.venue_fee_for(mint)),
                creator,
            })
            .await
        {
            Ok((position, bundle)) => {
                // Second fill is real custody: park it.
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
                // WAL first.
                trader.acknowledge_buy(&position, &bundle);
                self.exec.insert_tracked(position).await;
                self.exec.clear_diverged(mint).await;
                // First-touch cost feeds the day-loss gate.
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
                // No signature: fill unknown, park manual.
                warn!(%mint, error = %reason, "LIVE buy dispatch unknown; parked for manual review (may hold — check mint activity on explorer, no signature exists)");
                trader.record_manual(&position, "dispatch-unknown");
                self.exec.park_manual(mint.to_string(), *position).await;
                self.emit(format!("LIVE buy dispatch unknown on {mint}; MANUAL REVIEW REQUIRED (fill unknown)"));
            }
            Err(LiveError::Unconfirmed(msg))
                if msg.contains("Confirmed failure:") =>
            {
                // Proven failure: no fill, paper diverged.
                warn!(%mint, error = %msg, "LIVE buy CONFIRMED-FAILED on-chain (no fill); paper still open but DIVERGED");
                self.exec.mark_diverged(mint, msg).await;
                self.emit(format!("LIVE buy confirmed-failed on {mint}; paper open WITHOUT live cover"));
            }
            Err(LiveError::Unconfirmed(msg)) => {
                // Pending stays with recheck; GiveUp parks it.
                warn!(%mint, error = %msg, "LIVE buy unconfirmed; tracked as pending, rechecking");
            }
            Err(e) => {
                // No fill: paper stays open but diverged.
                warn!(%mint, error = %e, "LIVE buy FAILED (no fill); paper position still open but DIVERGED");
                self.exec.mark_diverged(mint, e.to_string()).await;
                self.emit(format!("LIVE buy failed on {mint} ({e}); paper open WITHOUT live cover"));
            }
        }
    }

    /// Re-poll pending buys. No-op when empty.
    pub(crate) async fn recheck_live_pending(&self) {
        let Some(trader) = &self.exec.live else { return };
        if !trader.has_pending() {
            return;
        }
        for recheck in trader.recheck_pending().await {
            let mint = recheck.position.mint.clone();
            match recheck.outcome {
                RecheckOutcome::Confirmed => {
                    // Late confirm never clobbers a newer bag.
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
                    // WAL first, then track.
                    trader.acknowledge_buy(&recheck.position, &recheck.bundle_id);
                    self.exec.insert_tracked(recheck.position).await;
                    self.exec.clear_diverged(&mint).await;
                    // Same first-touch cost; stale feed skips.
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
                    // No verdict: may hold, park manual.
                    warn!(mint = %mint, bundle = %recheck.bundle_id, sig = %recheck.signature, "pending LIVE buy unconfirmed after retries; parked for MANUAL REVIEW (may hold — look up sig on explorer)");
                    trader.record_manual(&recheck.position, &recheck.signature);
                    self.exec.park_manual(mint.clone(), recheck.position).await;
                    self.emit(format!("LIVE buy unconfirmed on {mint} (sig {}); MANUAL REVIEW REQUIRED (may hold)", recheck.signature));
                }
            }
        }
    }

    /// Flush deferred PnL once live settles. Synthetic is dropped.
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

    /// Mirror paper closes with sells. True means settled.
    pub(crate) async fn live_sell(&self, mint: &str, price_usd: f64, liquidity_usd: f64, fraction_closed: f64) -> bool {
        let Some(trader) = &self.exec.live else { return true };
        let Some(sol_usd) = self.sol_feed.price() else {
            warn!(%mint, "stale SOL/USD — live sell skipped (no static fallback); will retry next tick");
            self.emit(format!("stale SOL/USD — live sell stalled on {mint}; exits resume when feed recovers"));
            return !self.is_live_tracked(trader, mint).await;
        };
        // Per-mint lock; other mints exit freely.
        let serial = Self::lock_for(&self.exec.live_sell_locks, mint).await;
        let _serial = serial.lock().await;
        // Cooldown plus budget: success resets.
        let now = now_secs();
        let cooling = self
            .exec
            .cooling(mint, now, SELL_RETRY_COOLDOWN.as_secs())
            .await;
        if cooling {
            return false;
        }
        // Floor at the mint tier (paper parity).
        let venue_fee_bps = self.portfolio.lock().await.venue_fee_for(mint);
        let probe = {
            let Some(tracked) = self.exec.tracked(mint).await else {
                self.flush_deferred_pnl(mint).await;
                return true;
            };
            // Floored math (oversell is unauthorized).
            let numer = (fraction_closed.clamp(0.0, 1.0) * 10_000.0).floor() as u64;
            let held = tracked.amount_tokens;
            let sell_amount = ((held as u128 * numer as u128) / 10_000u128) as u64;
            if sell_amount > held {
                warn!(mint = %mint, sell_amount, held, "live sell sizing overflow; refusing");
                return false;
            }
            // Floor only (never ceiling). Dust counts toward the retry
            // budget like any failed probe (note_failure stamps the
            // cooldown too), so an unsellable remainder parks to manual
            // after the budget instead of re-probing forever.
            if sell_amount == 0 {
                let attempts = self.exec.note_failure(mint, now).await;
                warn!(mint = %mint, fraction_closed, held, attempts, "live sell sizes to zero dust; holding, will re-probe until budget, then manual");
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
        // Validated pool leg means curve is gone: route Raydium.
        let pool = self
            .followed
            .lock()
            .await
            .get(mint)
            .and_then(|state| state.pool.clone());
        // Tip escalates with prior failures (bounded congestion
        // response; floors never widen instead). Read before the probe:
        // short lock, never held across awaits.
        let tip = escalated_tip(trader.base_tip_lamports(), self.exec.retry_attempts(mint).await);
        // Never compete with our own inflight bundle: a resend while
        // the prior sell is still engine-pending risks two live
        // bundles for one bag. Unknown status proceeds (exits must
        // not stall on a status outage).
        if self.exec.retry_attempts(mint).await > 0 && trader.prior_sell_pending(mint).await {
            warn!(%mint, "prior sell bundle still inflight-pending; holding resend");
            return false;
        }
        let sell_result = match &pool {
            Some(leg) => {
                warn!(%mint, amm = %leg.amm, leg_mint = %leg.mint, "routing post-migration exit via Raydium");
                trader.sell_raydium(&RaydiumSellParams {
                    position: &probe,
                    pool: leg,
                    tip_lamports: tip,
                }).await
            }
            None => {
                trader
                    .sell(&LiveSellParams {
                        position: &probe,
                        price_usd,
                        sol_usd,
                        liquidity_usd,
                        venue_fee_bps: Some(venue_fee_bps),
                        tip_lamports: tip,
                    })
                    .await
            }
        };
        match sell_result {
            Ok(bundle) => {
                // WAL first.
                trader.record_sell(mint, probe.amount_tokens, &bundle);
                self.exec.subtract_fill(mint, probe.amount_tokens).await;
                // Success resets the budget.
                self.exec.reset_retry(mint).await;
                self.flush_deferred_pnl(mint).await;
                self.maybe_release_follow(mint).await;
                // Dispatch cost at the ACTUAL tip: the day-loss gate
                // sees live bleed, not paper-only profit.
                let tip_usd = trader.dispatch_cost_for_tip(tip) as f64 / 1_000_000_000.0 * sol_usd;
                if tip_usd.is_finite() && tip_usd > 0.0 {
                    self.daily.lock().await.record(-tip_usd, now_secs());
                    self.portfolio.lock().await.debit_dispatch_cost(tip_usd);
                }
                warn!(%mint, %bundle, "LIVE sell dispatched via Jito (real funds)");
                self.emit(Alerter::format_live_sell(mint, &bundle));
                true
            }
            Err(e) => {
                // Deterministic rejects re-probe; unroutable parks after budget.
                let deterministic = matches!(
                    &e,
                    LiveError::BadOrder(_)
                        | LiveError::Dispatch(ExecutorError::BadOrder(_))
                );
                let attempts = self.exec.note_failure(mint, now).await;
                if attempts >= SELL_MAX_ATTEMPTS {
                    // Give up automation, park manual.
                    self.exec.reset_retry(mint).await;
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
                    // Auto-exit over (human owns it): paper PnL may settle.
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

    /// Operator full exit (`sell <mint>`). True means settled.
    pub async fn manual_sell(&self, mint: &str) -> bool {
        // Bad files die here, never retry.
        if executor::parse_pubkey(mint).is_err() {
            warn!(%mint, "manual sell request for a non-pubkey mint; dropping request");
            return true;
        }
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
            // Terminal empty: evict locks.
            self.evict_mint_locks(mint).await;
            self.portfolio.lock().await.clear_venue_fee_for_mint(mint);
            return true;
        };
        // Resurrect parked bags so `live_sell` can probe them. Only the
        // exact key resurrects (one tracked bag per mint); suffixed
        // duplicates stay parked and are reported loudly instead of
        // rotting silently — exit each explicitly.
        if !self.exec.is_tracked(mint).await
            && let Some(held) = self.exec.take_manual_exact(mint).await
        {
            warn!(%mint, amount = held.amount_tokens, "manual sell resurrects a parked bag for explicit exit");
            self.exec.insert_tracked(held).await;
        }
        let parked_dups: Vec<String> = self
            .exec
            .manual_keys()
            .await
            .into_iter()
            .filter(|k| k != mint && live_base_mint(k) == mint)
            .collect();
        if !parked_dups.is_empty() {
            warn!(%mint, dups = ?parked_dups, "other parked bags for this mint stay parked; exit each explicitly");
            self.emit(format!(
                "manual sell on {mint} leaves parked duplicates ({}) standing by; exit each explicitly",
                parked_dups.join(", ")
            ));
        }
        let tick = match self.reserves.fetch_tick(mint, &curve).await {
            Ok(tick) => tick,
            Err(CurveError::BadMintKey(_)) | Err(CurveError::BadCurveKey(_)) => {
                warn!(%mint, "manual sell request unroutable; dropping request");
                // Terminal: evict locks and tier.
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
        // Live first, breaker second.
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
            // Settle deferred PnL only when no live exposure remains.
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
        LiveExecution::paper().with_live(None)
    }

    #[tokio::test]
    async fn tracked_lifecycle_insert_subtract_remove() {
        let exec = exec();
        assert!(!exec.is_tracked("MINT").await);
        exec.insert_tracked(bag("MINT", 1_000)).await;
        assert!(exec.is_tracked("MINT").await);
        assert_eq!(exec.tracked("MINT").await.expect("bag").amount_tokens, 1_000);
        assert!(!exec.subtract_fill("MINT", 400).await);
        assert_eq!(exec.tracked("MINT").await.expect("bag").amount_tokens, 600);
        assert!(exec.subtract_fill("MINT", 600).await);
        assert!(!exec.is_tracked("MINT").await);
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
    async fn retry_budget_counts_failures() {
        let exec = exec();
        assert_eq!(exec.retry_attempts("MINT").await, 0);
        assert_eq!(exec.note_failure("MINT", 1_000).await, 1);
        assert_eq!(exec.retry_attempts("MINT").await, 1);
        assert_eq!(exec.note_failure("MINT", 1_001).await, 2);
        assert!(exec.cooling("MINT", 1_002, 30).await);
        assert!(!exec.cooling("MINT", 9_999, 30).await);
        exec.reset_retry("MINT").await;
        assert!(!exec.cooling("MINT", 2_002, 30).await);
        assert_eq!(exec.retry_attempts("MINT").await, 0);
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
        assert_eq!(exec.take_deferred("MINT").await, Some((7.0, 2)));
        assert!(exec.take_deferred("MINT").await.is_none());
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

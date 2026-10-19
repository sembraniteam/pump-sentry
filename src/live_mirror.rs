//! Live mirror: dispatch, rechecks, sells, manual exits.

use std::collections::{HashMap, VecDeque};

use executor::DeadReason;

/// One settled execution gap: paper-booked close vs live reality.
/// `settle_price_usd` is the tick price when live settled (the same feed
/// paper uses) — NOT the intra-bundle fill, which is unknowable without
/// parsing the landed tx. The gap therefore measures timing + tip drag
/// and never pretends to measure bundle slippage. Entry legs have
/// their own record (`EntryGap`, from landed balances — strictly more
/// direct). Read averages as a LOWER BOUND on live drag, never the total.
/// One settled exit gap: model close vs live settle, plus the
/// dimensions execution intelligence aggregates by. `venue` is the
/// dispatch path that settled (`curve` | `pumpswap` | `raydium`);
/// `liquidity_usd` is the SOL-side depth at settle time;
/// `notional_usd` is qty × paper price (the ticket size this gap
/// prices). Together they answer "what kind of trade bled" without
/// re-deriving anything outside this tracker. (No settle timestamp:
/// exit timing is pool-driven and never tabled — per-hour analysis
/// covers entries only; offline forensics keeps `JournalLine::Gap.ts`.)
#[derive(Debug, Clone)]
pub struct FillGap {
    pub mint: String,
    pub paper_price_usd: f64,
    pub settle_price_usd: f64,
    pub qty_tokens: u64,
    pub tip_usd: f64,
    /// `(settle - paper) * qty - tip`: negative means live did worse.
    /// Long-only pipeline, so the sign needs no side adjustment.
    pub gap_usd: f64,
    pub venue: String,
    pub liquidity_usd: f64,
    pub notional_usd: f64,
}

/// One settled entry gap: paper-booked entry vs landed fill.
/// Recorded directly at buy confirm (no pending matching — paper open
/// and live fill share one flow). Fill price comes from real pre/post
/// balances, so this leg is strictly more direct than exit gaps (which
/// proxy the settle via tick); only the USD conversion rides the feed.
/// Sign matches [`FillGap`]: negative means live did worse (paid more
/// here), so both heartbeat legs read the same way.
#[derive(Debug, Clone)]
pub struct EntryGap {
    pub mint: String,
    pub paper_price_usd: f64,
    pub fill_price_usd: f64,
    pub qty_tokens: f64,
    /// `(paper - fill) * qty`: negative means live paid more.
    pub gap_usd: f64,
    /// Dispatch path that filled (`curve` — buys never ride pools;
    /// recorded anyway so the schema stays uniform).
    pub venue: String,
    /// SOL-side depth at decision time.
    pub liquidity_usd: f64,
    /// Ticket size this gap prices (qty × paper price).
    pub notional_usd: f64,
    pub ts: u64,
}

/// `(count, avg, total)` over a retained window; zero-count yields zeros.
/// Single owner for both exit and entry aggregates.
fn summarize(count: usize, total_gap_usd: f64) -> (usize, f64, f64) {
    if count == 0 {
        (0, 0.0, 0.0)
    } else {
        (count, total_gap_usd / count as f64, total_gap_usd)
    }
}
/// Bounded execution-gap log with aggregates for heartbeat calibration.
/// At most `FILL_GAP_CAP` entries; oldest shed first. This is a
/// calibration signal, not custody: shedding loses precision, never
/// funds. `None` settle (no pending paper note) records nothing —
/// unmatched settles are orphan/manual legs, never fabricated gaps.
pub struct FillGapTracker {
    pending: HashMap<String, VecDeque<(f64, u64)>>,
    gaps: VecDeque<FillGap>,
    total_gap_usd: f64,
    entry_gaps: VecDeque<EntryGap>,
    entry_total_gap_usd: f64,
}

/// Pending notes kept per mint. Moonbag scale-outs close twice per
/// position (plus re-entries after cooldown), so one slot per mint
/// orphaned every second leg; a short FIFO covers the realistic
/// depth while staying bounded.
const MAX_PENDING_NOTES_PER_MINT: usize = 8;

/// Gap history cap (calibration window, not a position limit).
pub const FILL_GAP_CAP: usize = 128;

/// Notional (USD) bucket bounds: 5 buckets from 4 edges. Sized for
/// $10–75 tickets: sub-$10 dust, retail bands, whale.
const SIZE_BOUNDS_USD: [f64; 4] = [10.0, 25.0, 50.0, 100.0];
/// Liquidity (USD) bucket bounds: 4 buckets from 3 edges, log-spaced
/// across drained-to-deep curves.
const LIQ_BOUNDS_USD: [f64; 3] = [1_000.0, 10_000.0, 100_000.0];
/// Samples below this in a cell read as "unknown", never as zero:
/// a thin cell must not refuse entries (or reassure about them).
pub const MIN_GAP_SAMPLES: usize = 3;

/// Bucket index: first bound the value stays under, else last bucket.
/// Non-finite/negative values bucket 0 (drained/unknown reads fail
/// closed downstream anyway; here they just don't earn precision).
fn size_bucket(notional_usd: f64) -> usize {
    if !notional_usd.is_finite() {
        return 0;
    }
    SIZE_BOUNDS_USD
        .iter()
        .position(|b| notional_usd < *b)
        .unwrap_or(SIZE_BOUNDS_USD.len())
}

fn liq_bucket(liquidity_usd: f64) -> usize {
    if !liquidity_usd.is_finite() {
        return 0;
    }
    LIQ_BOUNDS_USD
        .iter()
        .position(|b| liquidity_usd < *b)
        .unwrap_or(LIQ_BOUNDS_USD.len())
}

/// Hour of day (UTC) for timing analysis.
fn hour_of(ts: u64) -> usize {
    (ts / 3_600 % 24) as usize
}

/// One aggregated cell: borrowed view over a window scan, never
/// stored — the windows stay the single source, so staleness can only
/// ever be as old as `FILL_GAP_CAP` settles.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GapCell {
    pub n: usize,
    pub avg_gap_usd: f64,
    pub total_gap_usd: f64,
}

fn cell_of(n: usize, total: f64) -> Option<GapCell> {
    if n == 0 {
        return None;
    }
    Some(GapCell {
        n,
        avg_gap_usd: total / n as f64,
        total_gap_usd: total,
    })
}

impl FillGapTracker {
    /// Empty tracker.
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            gaps: VecDeque::new(),
            total_gap_usd: 0.0,
            entry_gaps: VecDeque::new(),
            entry_total_gap_usd: 0.0,
        }
    }

    /// Paper booked a close at the model price. Notes queue per mint
    /// (FIFO): moonbag scale-outs and re-entries close the same mint
    /// repeatedly, and each settle consumes the oldest unmatched note.
    /// Past `MAX_PENDING_NOTES_PER_MINT` the oldest sheds (bounded
    /// calibration data, documented imprecision).
    pub fn note_paper_close(&mut self, mint: &str, paper_price_usd: f64, qty_tokens: u64) {
        if paper_price_usd.is_finite() && paper_price_usd > 0.0 && qty_tokens > 0 {
            let queue = self.pending.entry(mint.to_string()).or_default();
            queue.push_back((paper_price_usd, qty_tokens));
            while queue.len() > MAX_PENDING_NOTES_PER_MINT {
                queue.pop_front();
            }
        }
    }

    /// Live settled a sale. Returns the gap when a paper close was
    /// pending; `None` settles nothing. Consumes the oldest unmatched
    /// note (FIFO). Non-finite settles keep the pending notes for the
    /// next round instead of recording garbage. Venue and liquidity
    /// are observed at settle (the dispatch path just ran, the tick
    /// mark is fresh) — never reconstructed later.
    pub fn note_live_settle(
        &mut self,
        mint: &str,
        settle_price_usd: f64,
        tip_usd: f64,
        venue: &str,
        liquidity_usd: f64,
    ) -> Option<FillGap> {
        if !settle_price_usd.is_finite() || !tip_usd.is_finite() {
            return None;
        }
        let queue = self.pending.get_mut(mint)?;
        let (paper_price_usd, qty_tokens) = queue.pop_front()?;
        if queue.is_empty() {
            self.pending.remove(mint);
        }
        let gap_usd = (settle_price_usd - paper_price_usd) * qty_tokens as f64 - tip_usd;
        if !gap_usd.is_finite() {
            return None;
        }
        let gap = FillGap {
            mint: mint.to_string(),
            paper_price_usd,
            settle_price_usd,
            qty_tokens,
            tip_usd,
            gap_usd,
            venue: venue.to_string(),
            liquidity_usd,
            notional_usd: qty_tokens as f64 * paper_price_usd,
        };
        self.total_gap_usd += gap_usd;
        self.gaps.push_back(gap.clone());
        while self.gaps.len() > FILL_GAP_CAP {
            self.gaps.pop_front();
        }
        Some(gap)
    }

    /// `(settled_count, avg_gap_usd, total_gap_usd)` for the heartbeat.
    /// Average over the retained window; zero-count yields zeros.
    pub fn stats(&self) -> (usize, f64, f64) {
        summarize(self.gaps.len(), self.total_gap_usd)
    }

    /// Record a landed entry gap. Single owner of entry gap math:
    /// callers pass observation + paper quote, never a precomputed gap.
    /// `None` (returned as no record) on any non-finite input or
    /// non-positive price/qty — calibration skips, never fabricates.
    /// Seven observation fields ride one call (paper + fill + shape);
    /// the struct would split what the tracker owns together.
    #[allow(clippy::too_many_arguments)]
    pub fn note_entry_gap(
        &mut self,
        mint: &str,
        paper_price_usd: f64,
        fill_price_usd: f64,
        qty_tokens: f64,
        venue: &str,
        liquidity_usd: f64,
        ts: u64,
    ) -> Option<EntryGap> {
        if !paper_price_usd.is_finite()
            || !fill_price_usd.is_finite()
            || !qty_tokens.is_finite()
            || paper_price_usd <= 0.0
            || fill_price_usd <= 0.0
            || qty_tokens <= 0.0
        {
            return None;
        }
        let gap_usd = (paper_price_usd - fill_price_usd) * qty_tokens;
        if !gap_usd.is_finite() {
            return None;
        }
        let gap = EntryGap {
            mint: mint.to_string(),
            paper_price_usd,
            fill_price_usd,
            qty_tokens,
            gap_usd,
            venue: venue.to_string(),
            liquidity_usd,
            notional_usd: qty_tokens * paper_price_usd,
            ts,
        };
        self.entry_total_gap_usd += gap_usd;
        self.entry_gaps.push_back(gap.clone());
        while self.entry_gaps.len() > FILL_GAP_CAP {
            self.entry_gaps.pop_front();
        }
        Some(gap)
    }

    /// `(filled_count, avg_gap_usd, total_gap_usd)` for the heartbeat.
    pub fn entry_stats(&self) -> (usize, f64, f64) {
        summarize(self.entry_gaps.len(), self.entry_total_gap_usd)
    }

    /// Per-venue exit aggregates over the retained window, in first-seen
    /// order: `(venue, n, avg, total)`. Derived from the window on every
    /// call (O(cap)) — no parallel counters to drift or go stale.
    pub fn venue_exit_stats(&self) -> Vec<(String, usize, f64, f64)> {
        let mut order: Vec<String> = Vec::new();
        let mut acc: HashMap<String, (usize, f64)> = HashMap::new();
        for gap in &self.gaps {
            let entry = acc.entry(gap.venue.clone()).or_insert((0, 0.0));
            entry.0 += 1;
            entry.1 += gap.gap_usd;
            if !order.contains(&gap.venue) {
                order.push(gap.venue.clone());
            }
        }
        order
            .into_iter()
            .filter_map(|venue| {
                let (n, total) = acc.remove(&venue)?;
                cell_of(n, total).map(|c| (venue, c.n, c.avg_gap_usd, c.total_gap_usd))
            })
            .collect()
    }

    /// Per-venue entry aggregates, same shape as [`Self::venue_exit_stats`].
    pub fn venue_entry_stats(&self) -> Vec<(String, usize, f64, f64)> {
        let mut order: Vec<String> = Vec::new();
        let mut acc: HashMap<String, (usize, f64)> = HashMap::new();
        for gap in &self.entry_gaps {
            let entry = acc.entry(gap.venue.clone()).or_insert((0, 0.0));
            entry.0 += 1;
            entry.1 += gap.gap_usd;
            if !order.contains(&gap.venue) {
                order.push(gap.venue.clone());
            }
        }
        order
            .into_iter()
            .filter_map(|venue| {
                let (n, total) = acc.remove(&venue)?;
                cell_of(n, total).map(|c| (venue, c.n, c.avg_gap_usd, c.total_gap_usd))
            })
            .collect()
    }

    /// Costliest (venue, size, liquidity) exit cell in the window:
    /// `(venue, size_idx, liq_idx, cell)`. Cells below
    /// [`MIN_GAP_SAMPLES`] never win — a thin cell must not page the
    /// operator or refuse entries. `None` when no cell qualifies.
    pub fn worst_exit_cell(&self) -> Option<(String, usize, usize, GapCell)> {
        let mut cells: HashMap<(String, usize, usize), (usize, f64)> = HashMap::new();
        for gap in &self.gaps {
            let key = (
                gap.venue.clone(),
                size_bucket(gap.notional_usd),
                liq_bucket(gap.liquidity_usd),
            );
            let entry = cells.entry(key).or_insert((0, 0.0));
            entry.0 += 1;
            entry.1 += gap.gap_usd;
        }
        cells
            .into_iter()
            .filter(|(_, (n, _))| *n >= MIN_GAP_SAMPLES)
            .filter_map(|((venue, size, liq), (n, total))| {
                cell_of(n, total).map(|c| (venue, size, liq, c))
            })
            .min_by(|a, b| {
                a.3
                    .avg_gap_usd
                    .partial_cmp(&b.3.avg_gap_usd)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    }

    /// Same as [`Self::worst_exit_cell`] for the entry window.
    pub fn worst_entry_cell(&self) -> Option<(String, usize, usize, GapCell)> {
        let mut cells: HashMap<(String, usize, usize), (usize, f64)> = HashMap::new();
        for gap in &self.entry_gaps {
            let key = (
                gap.venue.clone(),
                size_bucket(gap.notional_usd),
                liq_bucket(gap.liquidity_usd),
            );
            let entry = cells.entry(key).or_insert((0, 0.0));
            entry.0 += 1;
            entry.1 += gap.gap_usd;
        }
        cells
            .into_iter()
            .filter(|(_, (n, _))| *n >= MIN_GAP_SAMPLES)
            .filter_map(|((venue, size, liq), (n, total))| {
                cell_of(n, total).map(|c| (venue, size, liq, c))
            })
            .min_by(|a, b| {
                a.3
                    .avg_gap_usd
                    .partial_cmp(&b.3.avg_gap_usd)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    }

    /// Expected entry gap (USD) for a ticket of this shape: the cell
    /// average, or `None` below [`MIN_GAP_SAMPLES`] (unknown reads as
    /// unknown — the entry gate allows on unknown, refuses only on
    /// proven bleed). Entry window only: exits never block.
    pub fn expected_entry_gap_usd(
        &self,
        venue: &str,
        notional_usd: f64,
        liquidity_usd: f64,
    ) -> Option<f64> {
        let (mut n, mut total) = (0usize, 0.0f64);
        let (size, liq) = (size_bucket(notional_usd), liq_bucket(liquidity_usd));
        for gap in &self.entry_gaps {
            if gap.venue == venue
                && size_bucket(gap.notional_usd) == size
                && liq_bucket(gap.liquidity_usd) == liq
            {
                n += 1;
                total += gap.gap_usd;
            }
        }
        if n < MIN_GAP_SAMPLES {
            return None;
        }
        Some(total / n as f64)
    }

    /// Hourly entry-gap aggregates (UTC hour → cell), for timing
    /// analysis. Exit timing is pool-driven and never gates entries,
    /// so only the entry window is tabled.
    pub fn entry_hour_stats(&self) -> [(usize, f64, f64); 24] {
        let mut acc = [(0usize, 0.0f64); 24];
        for gap in &self.entry_gaps {
            let slot = &mut acc[hour_of(gap.ts)];
            slot.0 += 1;
            slot.1 += gap.gap_usd;
        }
        let mut out = [(0usize, 0.0f64, 0.0f64); 24];
        for (i, (n, total)) in acc.iter().enumerate() {
            out[i] = if *n == 0 {
                (0, 0.0, 0.0)
            } else {
                (*n, total / *n as f64, *total)
            };
        }
        out
    }

    /// Drop a pending note without settling (lifecycle eviction).
    pub fn pending_remove(&mut self, mint: &str) {
        self.pending.remove(mint);
    }
}

impl Default for FillGapTracker {
    fn default() -> Self {
        Self::new()
    }
}

use std::sync::Arc;

use alert::Alerter;
use curve::CurveError;
use executor::ExecutorError;
use simulator::ExitReason;
use solana_pubkey::Pubkey;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::journal::{FillClaim, JournalLine};
use crate::live::{
    DecimalsRefusal, LAMPORTS_PER_SOL, LiveBuyParams, LiveError, LivePosition, LiveSellParams,
    LiveTrader, PumpSwapSellParams, RaydiumSellParams, RecheckOutcome, SELL_MAX_ATTEMPTS,
    SELL_RETRY_COOLDOWN, six_decimals,
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

/// Breaker headline: paper net less dispatch bleed. A non-finite
/// dispatch leg never poisons the gate; falls back to raw net.
/// Single owner for the `net - dispatch` convention used by both
/// the tick path and manual sells.
pub(crate) fn breaker_headline(net_pnl: f64, dispatch_usd: f64) -> f64 {
    if dispatch_usd.is_finite() {
        net_pnl - dispatch_usd
    } else {
        net_pnl
    }
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
    /// GiveUp timestamps (secs) for burst alerting. Pruned on read.
    pub giveup_stamps: Mutex<Vec<u64>>,
    /// Paper-vs-live execution gaps (calibration signal).
    pub gaps: Mutex<FillGapTracker>,
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
            giveup_stamps: Mutex::new(Vec::new()),
            gaps: Mutex::new(FillGapTracker::new()),
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
        let key = position.mint.to_string();
        self.live_positions.lock().await.insert(key, position);
    }

    /// Remove and return the fill, if any.
    pub async fn remove_tracked(&self, mint: &str) -> Option<LivePosition> {
        self.live_positions.lock().await.remove(mint)
    }

    /// Subtract a confirmed sell; true when the bag emptied.
    /// A missing bag is custody drift (dispatch confirmed what we no
    /// longer track): loud, then treated as settled so the breaker
    /// still resolves instead of deferring forever. An amount larger
    /// than tracked is also drift, but the other way: zeroing the bag
    /// would hide it, so refuse (false) and keep investigating.
    pub async fn subtract_fill(&self, mint: &str, amount: u64) -> bool {
        let mut guard = self.live_positions.lock().await;
        match guard.get_mut(mint) {
            Some(tracked) => {
                let Some(remaining) = tracked.amount_tokens.checked_sub(amount) else {
                    warn!(%mint, amount, held = tracked.amount_tokens, "sell confirmed more than tracked; refusing to zero the bag");
                    return false;
                };
                tracked.amount_tokens = remaining;
                if remaining == 0 {
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
    pub async fn manual_curve(&self, mint: &str) -> Option<Pubkey> {
        self.live_manual
            .lock()
            .await
            .get(mint)
            .map(|held| held.curve)
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

    /// Distinct base mints with live exposure (tracked, parked, or
    /// pending buys). Heat counts these alongside paper positions:
    /// paper cash recycles on close while live SOL stays locked, so
    /// paper-only heat would keep opening into a stuck, diverged book.
    pub async fn held_base_mints(&self) -> std::collections::HashSet<String>
    where
        L: LiveSink,
    {
        let mut out = std::collections::HashSet::new();
        for key in self.live_positions.lock().await.keys() {
            out.insert(live_base_mint(key).to_string());
        }
        for key in self.live_manual.lock().await.keys() {
            out.insert(live_base_mint(key).to_string());
        }
        if let Some(trader) = self.live.as_ref() {
            out.extend(trader.pending_mints());
        }
        out
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

    /// `(mints, total)` stashed while live holds: paper PnL kept out
    /// of the breaker. Invisible stashes hide real exposure, so the
    /// heartbeat carries both numbers.
    pub async fn deferred_stats(&self) -> (usize, f64) {
        let guard = self.deferred_pnl.lock().await;
        let total: f64 = guard.values().map(|(t, _)| *t).sum();
        (guard.len(), total)
    }

    /// Override-priced mints excluded from the breaker.
    pub async fn synthetic_count(&self) -> usize {
        self.synthetic_mints.lock().await.len()
    }

    /// Stamp a GiveUp (buy-unconfirmed or sell-budget-exhausted) for
    /// burst alerting. Custody parking itself is unchanged.
    pub async fn note_giveup(&self, now_secs: u64) {
        self.giveup_stamps.lock().await.push(now_secs);
    }

    /// GiveUps within the trailing window (prunes older stamps).
    pub async fn giveups_since(&self, window_secs: u64, now_secs: u64) -> usize {
        let cutoff = now_secs.saturating_sub(window_secs);
        let mut stamps = self.giveup_stamps.lock().await;
        stamps.retain(|ts| *ts >= cutoff);
        stamps.len()
    }

    /// Paper booked a close at the model price. Notes queue per mint
    /// (FIFO); each live settle consumes the oldest unmatched note.
    /// Paper-only mode records nothing (no settlement exists to match).
    pub async fn note_gap_paper_close(&self, mint: &str, paper_price_usd: f64, qty_tokens: u64) {
        if self.live.is_none() {
            return;
        }
        self.gaps
            .lock()
            .await
            .note_paper_close(mint, paper_price_usd, qty_tokens);
    }

    /// Live settled a sale (call on every confirmed sell AND settled
    /// manual exit). Returns the gap when a paper close was pending
    /// (journal mirrors it); silent `None` otherwise (orphan legs
    /// record nothing — never fabricated gaps).
    pub async fn note_gap_live_settle(
        &self,
        mint: &str,
        settle_price_usd: f64,
        tip_usd: f64,
        venue: &str,
        liquidity_usd: f64,
    ) -> Option<FillGap> {
        let gap = self.gaps.lock().await.note_live_settle(
            mint,
            settle_price_usd,
            tip_usd,
            venue,
            liquidity_usd,
        );
        if let Some(ref gap) = gap {
            info!(mint = %gap.mint, paper = gap.paper_price_usd, settle = gap.settle_price_usd,
                qty = gap.qty_tokens, tip = format!("{:.4}", gap.tip_usd),
                gap_usd = format!("{:+.4}", gap.gap_usd),
                "execution gap settled (paper model vs live reality)");
        }
        gap
    }

    /// Drop a pending gap note (mint lifecycle ended with no settle:
    /// diverged paper, drained book). Calibration data, not custody.
    pub async fn clear_gap(&self, mint: &str) {
        self.gaps.lock().await.pending_remove(mint);
    }

    /// `(settled_count, avg_gap_usd, total_gap_usd)` for the heartbeat.
    pub async fn gap_stats(&self) -> (usize, f64, f64) {
        self.gaps.lock().await.stats()
    }

    /// Record a landed entry gap. Single call site per buy confirm;
    /// returns the gap for the journal mirror, silent `None` on
    /// degenerate input (orphan legs record nothing).
    pub async fn note_entry_gap(
        &self,
        mint: &str,
        paper_price_usd: f64,
        fill_price_usd: f64,
        qty_tokens: f64,
        venue: &str,
        liquidity_usd: f64,
    ) -> Option<EntryGap> {
        let gap = self.gaps.lock().await.note_entry_gap(
            mint,
            paper_price_usd,
            fill_price_usd,
            qty_tokens,
            venue,
            liquidity_usd,
            crate::settings::now_secs(),
        );
        if let Some(ref gap) = gap {
            info!(mint = %gap.mint, paper = gap.paper_price_usd, fill = gap.fill_price_usd,
                qty = gap.qty_tokens,
                gap_usd = format!("{:+.4}", gap.gap_usd),
                "entry gap settled (paper model vs landed fill)");
        }
        gap
    }

    /// `(filled_count, avg_gap_usd, total_gap_usd)` for the heartbeat.
    pub async fn entry_gap_stats(&self) -> (usize, f64, f64) {
        self.gaps.lock().await.entry_stats()
    }

    /// Per-venue exit aggregates for the heartbeat: `(venue, n, avg, total)`.
    pub async fn venue_exit_stats(&self) -> Vec<(String, usize, f64, f64)> {
        self.gaps.lock().await.venue_exit_stats()
    }

    /// Per-venue entry aggregates for the heartbeat.
    pub async fn venue_entry_stats(&self) -> Vec<(String, usize, f64, f64)> {
        self.gaps.lock().await.venue_entry_stats()
    }

    /// Costliest exit cell `(venue, size_idx, liq_idx, cell)`; `None`
    /// when no cell has enough samples to trust.
    pub async fn worst_exit_cell(&self) -> Option<(String, usize, usize, GapCell)> {
        self.gaps.lock().await.worst_exit_cell()
    }

    /// Costliest entry cell; `None` when no cell qualifies.
    pub async fn worst_entry_cell(&self) -> Option<(String, usize, usize, GapCell)> {
        self.gaps.lock().await.worst_entry_cell()
    }

    /// Hourly entry aggregates (UTC hour → `(n, avg, total)`).
    pub async fn entry_hour_stats(&self) -> [(usize, f64, f64); 24] {
        self.gaps.lock().await.entry_hour_stats()
    }

    /// Expected entry gap (USD) for a ticket of this shape, or `None`
    /// below [`MIN_GAP_SAMPLES`] (unknown is unknown — the entry gate
    /// allows on unknown). Entry window only: exits never block.
    pub async fn expected_entry_gap_usd(
        &self,
        venue: &str,
        notional_usd: f64,
        liquidity_usd: f64,
    ) -> Option<f64> {
        self.gaps
            .lock()
            .await
            .expected_entry_gap_usd(venue, notional_usd, liquidity_usd)
    }

    /// Pending-buy count for the heartbeat (unconfirmed fills).
    pub fn live_pending_count(&self) -> usize
    where
        L: LiveSink,
    {
        self.live
            .as_ref()
            .map(|t| t.pending_mints().len())
            .unwrap_or(0)
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
        trader.has_pending_for(mint)
    }

    /// Mirror a paper open with a real buy. Fails closed on stale SOL.
    /// `mark` is the paper book's own entry mark: the dispatch quotes
    /// exactly the state the position was booked on.
    pub(crate) async fn live_buy(
        &self,
        mint: &str,
        curve: &str,
        assoc_curve: &str,
        mark: simulator::Mark,
        sol: Option<(f64, u64)>,
        creator: Option<&str>,
    ) {
        let Some(trader) = &self.exec.live else { return };
        let entry_quote = mark.price;
        let Some((sol_usd, _ts)) = sol.filter(|(_, ts)| self.sol_feed.age_ok(*ts)) else {
            warn!(%mint, "stale SOL/USD — live buy skipped (no static fallback)");
            // One-shot buy: a skip leaves paper uncovered.
            self.journal_buy_skip(mint, "stale sol");
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
                self.journal_buy_skip(mint, "heat limit");
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
            self.journal_buy_skip(mint, "already tracked");
            self.emit(format!("LIVE buy skipped on already-tracked {mint}; no dispatch"));
            return;
        }
        // Decimals explicit (cached read): sizing assumes per-mint units.
        // Gate drops non-6; live re-verifies so a 9-decimal mint can
        // never size 1000x wrong here.
        let decimals = match six_decimals(&self.reserves, mint).await {
            Ok(d) => d,
            Err(DecimalsRefusal::NonSix(other)) => {
                warn!(%mint, decimals = other, "non-6-decimal mint; live buy skipped (fail-closed)");
                self.journal_buy_skip(mint, "non-6 decimals");
                self.exec
                    .mark_diverged(mint, "live buy skipped: non-6 decimals".to_string())
                    .await;
                self.emit(format!("LIVE buy skipped on {mint} (non-6 decimals); paper open WITHOUT live cover"));
                return;
            }
            Err(DecimalsRefusal::Unreadable(error)) => {
                warn!(%mint, %error, "mint decimals unreadable; live buy skipped (fail-closed)");
                self.journal_buy_skip(mint, "decimals unreadable");
                self.exec
                    .mark_diverged(mint, "live buy skipped: decimals unreadable".to_string())
                    .await;
                self.emit(format!("LIVE buy skipped on {mint} (decimals unreadable); paper open WITHOUT live cover"));
                return;
            }
        };
        // Parse once at the boundary: downstream custody and dispatch
        // take validated keys and never re-parse strings.
        let parsed = |label: &str, v: &str| match executor::parse_pubkey(v) {
            Ok(p) => Some(p),
            Err(e) => {
                warn!(%mint, %label, error = %e, "unparseable address; live buy skipped (fail-closed)");
                None
            }
        };
        let (Some(mint_key), Some(curve_key), Some(assoc_key)) =
            (parsed("mint", mint), parsed("curve", curve), parsed("assoc_curve", assoc_curve))
        else {
            self.exec
                .mark_diverged(mint, "live buy skipped: unparseable address".to_string())
                .await;
            self.emit(format!("LIVE buy skipped on {mint} (bad address); paper open WITHOUT live cover"));
            return;
        };
        let creator_key = match creator {
            Some(c) => match parsed("creator", c) {
                Some(p) => Some(p),
                None => {
                    self.exec
                        .mark_diverged(mint, "live buy skipped: bad creator".to_string())
                        .await;
                    self.emit(format!("LIVE buy skipped on {mint} (bad creator); paper open WITHOUT live cover"));
                    return;
                }
            },
            None => None,
        };
        // Read the tier BEFORE the match: a lock taken inside the
        // scrutinee (or its arguments) stays held through every arm,
        // and the cost-debit arms lock the book again — hoisted, the
        // guard drops here.
        let venue_fee_bps = self.portfolio.lock().await.venue_fee_for(mint);
        match trader
            .buy(&LiveBuyParams {
                mint: mint_key,
                curve: curve_key,
                assoc_curve: assoc_key,
                notional_usd: self.settings.notional_usd,
                mark,
                sol_usd,
                // Same tier as paper: sizing matches booked fills.
                venue_fee_bps: Some(venue_fee_bps),
                creator: creator_key,
                decimals,
            })
            .await
        {
            Ok((position, bundle, entry_fill)) => {
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
                // Entry-gap calibration: paper model vs landed fill.
                // Skipped when unobservable (late-confirm path never
                // carries one either) — absence is documented, not hidden.
                let entry_gap = if let Some(fill) = entry_fill {
                    self.exec.note_entry_gap(
                        mint,
                        entry_quote,
                        fill.fill_price_usd,
                        fill.tokens,
                        // Buys never ride pool legs (no pool-buy path
                        // exists): the venue is structural, not read.
                        "curve",
                        mark.liquidity_usd,
                    )
                    .await
                } else {
                    None
                };
                if let (Some(j), Some(entry_gap)) = (self.journal(), entry_gap) {
                    j.append(&JournalLine::Gap {
                        ts: now_secs(),
                        mint: entry_gap.mint,
                        leg: "entry".to_string(),
                        paper_price_usd: entry_gap.paper_price_usd,
                        realized_price_usd: entry_gap.fill_price_usd,
                        qty_tokens: entry_gap.qty_tokens,
                        tip_usd: None,
                        gap_usd: entry_gap.gap_usd,
                        venue: entry_gap.venue,
                        liquidity_usd: entry_gap.liquidity_usd,
                    });
                }
                // First-touch cost feeds the day-loss gate.
                self.debit_dispatch_usd(Self::dispatch_usd(
                    trader.first_touch_cost_lamports(),
                    sol_usd,
                ))
                .await;
                // Dispatch record: what was sent, at what, costing what.
                if let Some(j) = self.journal() {
                    j.append(&JournalLine::BuyAttempt {
                        ts: now_secs(),
                        mint: mint.to_string(),
                        bundle: bundle.clone(),
                        tokens: amount,
                        fill: entry_fill.map(|f| FillClaim {
                            fill_price_usd: f.fill_price_usd,
                            tokens: f.tokens,
                        }),
                        debit_usd: Some(Self::dispatch_usd(
                            trader.first_touch_cost_lamports(),
                            sol_usd,
                        )),
                    });
                    // Terminal bookkeeping for the direct path: every
                    // buy gets exactly one outcome line, so confirmed
                    // counts include at-dispatch confirms (the debit is
                    // NOT repeated — the Attempt line owns it).
                    self.journal_buy_outcome(mint, Some(bundle.clone()), "confirmed", None, None);
                }
                warn!(%mint, "LIVE buy dispatched via Jito (real funds)");
                self.emit(Alerter::format_live_buy(&self.card_ctx(), mint, &bundle, amount));
            }
            Err(LiveError::UnknownDispatch { position, reason }) => {
                // No signature: fill unknown, park manual.
                warn!(%mint, error = %reason, "LIVE buy dispatch unknown; parked for manual review (may hold — check mint activity on explorer, no signature exists)");
                self.journal_buy_outcome(mint, None, "unknown", Some(reason.to_string()), None);
                trader.record_manual(&position, "dispatch-unknown");
                self.exec.park_manual(mint.to_string(), *position).await;
                self.emit(format!("LIVE buy dispatch unknown on {mint}; MANUAL REVIEW REQUIRED (fill unknown)"));
            }
            Err(LiveError::Dead { reason, context }) => {
                // Proven no-fill. Only an on-chain failure burned money:
                // the chain charged base + priority, and the tip transfer
                // rolled back with the failed transaction.
                // Never-forwarded and expired dispatches paid nothing.
                let debit = (reason == DeadReason::FailedOnChain)
                    .then(|| Self::dispatch_usd(trader.failed_dispatch_cost_lamports(), sol_usd));
                if let Some(usd) = debit {
                    self.debit_dispatch_usd(usd).await;
                }
                warn!(%mint, %reason, error = %context, "LIVE buy provably dead (no fill); paper still open but DIVERGED");
                self.journal_buy_outcome(mint, None, "dead", Some(context.clone()), debit);
                self.exec.mark_diverged(mint, context).await;
                self.emit(format!(
                    "LIVE buy provably dead on {mint} ({reason}); paper open WITHOUT live cover"
                ));
            }
            Err(LiveError::Unconfirmed(msg)) => {
                // Pending stays with recheck; GiveUp parks it.
                warn!(%mint, error = %msg, "LIVE buy unconfirmed; tracked as pending, rechecking");
                self.journal_buy_outcome(mint, None, "unknown", Some(msg), None);
            }
            Err(e) => {
                // No fill: paper stays open but diverged.
                warn!(%mint, error = %ids::strip_error_url(&e.to_string()), "LIVE buy FAILED (no fill); paper position still open but DIVERGED");
                self.journal_buy_outcome(mint, None, "unknown", Some(e.to_string()), None);
                self.exec.mark_diverged(mint, e.to_string()).await;
                self.emit(format!("LIVE buy failed on {mint} ({e}); paper open WITHOUT live cover"));
            }
        }
    }

    /// Decision record: live buy decided not to dispatch (reason is
    /// the decision). Analytics-only; never gates anything.
    fn journal_buy_skip(&self, mint: &str, reason: &str) {
        if let Some(j) = self.journal() {
            j.append(&JournalLine::BuySkip {
                ts: now_secs(),
                mint: mint.to_string(),
                reason: reason.to_string(),
            });
        }
    }

    /// Decision record: terminal/recheck state for a pending live buy.
    fn journal_buy_outcome(
        &self,
        mint: &str,
        bundle: Option<String>,
        verdict: &str,
        detail: Option<String>,
        debit_usd: Option<f64>,
    ) {
        if let Some(j) = self.journal() {
            j.append(&JournalLine::BuyOutcome {
                ts: now_secs(),
                mint: mint.to_string(),
                bundle,
                verdict: verdict.to_string(),
                detail,
                debit_usd,
            });
        }
    }

    /// Decision record: live sell automation given up (bag parked).
    fn journal_mint_giveup(&self, mint: &str, cause: &str) {
        if let Some(j) = self.journal() {
            j.append(&JournalLine::SellGiveUp {
                ts: now_secs(),
                mint: mint.to_string(),
                cause: cause.to_string(),
            });
        }
    }

    /// One real dispatch cost in USD at the actual SOL price. Returns
    /// NaN/zero for degenerate inputs (callers skip those).
    fn dispatch_usd(lamports: u64, sol_usd: f64) -> f64 {
        lamports as f64 / LAMPORTS_PER_SOL * sol_usd
    }

    /// Debit a real dispatch cost (USD) from paper cash and the day
    /// tally: same accounting as the paper bleed, at actuals. Zero and
    /// non-finite amounts skip (nothing to book).
    async fn debit_dispatch_usd(&self, usd: f64) {
        if usd.is_finite() && usd > 0.0 {
            self.daily.lock().await.record(-usd, now_secs());
            self.portfolio.lock().await.debit_dispatch_cost(usd);
        }
    }

    /// Re-poll pending buys. No-op when empty.
    pub(crate) async fn recheck_live_pending(&self) {
        let Some(trader) = &self.exec.live else { return };
        if !trader.has_pending() {
            return;
        }
        for recheck in trader.recheck_pending().await {
            // String-boundary key derived once; custody holds `Pubkey`s.
            let mint = recheck.position.mint.to_string();
            match recheck.outcome {
                RecheckOutcome::Confirmed => {
                    // Late confirm never clobbers a newer bag.
                    if self.is_live_tracked(trader, &mint).await {
                        warn!(mint = %mint, bundle = %recheck.bundle_id, amount = recheck.position.amount_tokens, "late LIVE buy confirm for already-tracked mint; parked as manual (still HELD), MANUAL REVIEW REQUIRED");
                        self.journal_buy_outcome(
                            &mint,
                            Some(recheck.bundle_id.clone()),
                            "confirmed",
                            Some("late confirm parked as manual (still held)".to_string()),
                            None,
                        );
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
                    let confirmed_debit = self.sol_feed.price().map(|sol_usd| {
                        Self::dispatch_usd(trader.first_touch_cost_lamports(), sol_usd)
                    });
                    if let Some(usd) = confirmed_debit {
                        self.debit_dispatch_usd(usd).await;
                    }
                    self.journal_buy_outcome(
                        &mint,
                        Some(recheck.bundle_id.clone()),
                        "confirmed",
                        None,
                        confirmed_debit,
                    );
                    warn!(mint = %mint, "LIVE buy confirmed late; now tracked with exits");
                    self.emit(Alerter::format_live_buy(&self.card_ctx(), &mint, &recheck.bundle_id, amount));
                }
                RecheckOutcome::Dead(reason) => {
                    // Same honest burn as the direct path: only an
                    // on-chain failure paid fees (the tip rolled back).
                    let dead_debit = (reason == DeadReason::FailedOnChain)
                        .then(|| self.sol_feed.price())
                        .flatten()
                        .map(|sol_usd| {
                            Self::dispatch_usd(trader.failed_dispatch_cost_lamports(), sol_usd)
                        });
                    if let Some(usd) = dead_debit {
                        self.debit_dispatch_usd(usd).await;
                    }
                    warn!(mint = %mint, bundle = %recheck.bundle_id, %reason, "pending LIVE buy provably dead; no fill, dropping");
                    self.journal_buy_outcome(
                        &mint,
                        Some(recheck.bundle_id.clone()),
                        "dead",
                        Some(reason.to_string()),
                        dead_debit,
                    );
                }
                RecheckOutcome::StillUnknown => {
                    warn!(mint = %mint, bundle = %recheck.bundle_id, "pending LIVE buy still unconfirmed; keeping for next round");
                    self.journal_buy_outcome(
                        &mint,
                        Some(recheck.bundle_id.clone()),
                        "unknown",
                        None,
                        None,
                    );
                }
                RecheckOutcome::GiveUp => {
                    // No verdict: may hold, park manual.
                    warn!(mint = %mint, bundle = %recheck.bundle_id, sig = %recheck.signature, "pending LIVE buy unconfirmed after retries; parked for MANUAL REVIEW (may hold — look up sig on explorer)");
                    self.journal_buy_outcome(
                        &mint,
                        Some(recheck.bundle_id.clone()),
                        "giveup",
                        Some(format!("sig {}", recheck.signature)),
                        None,
                    );
                    trader.record_manual(&recheck.position, &recheck.signature);
                    self.exec.park_manual(mint.clone(), recheck.position).await;
                    self.exec.note_giveup(now_secs()).await;
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

    /// Give up automation, park the tracked bag for manual exit.
    /// Always returns false: parked manual is still held, so the
    /// caller must NOT treat this as settled (paper PnL routes through
    /// the normal stash path instead of feeding the breaker for a bag
    /// that never exited — it settles when the manual exit clears).
    async fn park_for_manual_exit(&self, trader: &L, mint: &str, now: u64, cause: &str) -> bool
    where
        L: LiveSink,
    {
        self.exec.reset_retry(mint).await;
        if let Some(held) = self.exec.tracked(mint).await {
            trader.record_manual(&held, "sell-retries-exhausted");
        }
        if let Some(held) = self.exec.remove_tracked(mint).await {
            self.exec.park_manual(mint.to_string(), held).await;
        }
        self.exec.note_giveup(now).await;
        self.journal_mint_giveup(mint, cause);
        warn!(%mint, cause, "LIVE sell gave up; parked for manual exit (still HELD)");
        self.emit(format!("LIVE sell gave up on {mint}; MANUAL EXIT REQUIRED (bag still held)"));
        false
    }

    /// Mirror paper closes with sells. True means settled.
    pub(crate) async fn live_sell(
        &self,
        mint: &str,
        mark: &simulator::Mark,
        fraction_closed: f64,
    ) -> bool {
        let Some(trader) = &self.exec.live else { return true };
        // Local name for the body; exact reserves ride `mark.curve`.
        let price_usd = mark.price;
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
        // Decimals explicit (cached): floor math needs per-mint units.
        // Unreadable retries next tick; non-6 counts toward the retry
        // budget like dust (gate never opens those, so this is drift).
        let decimals = match six_decimals(&self.reserves, mint).await {
            Ok(d) => d,
            Err(DecimalsRefusal::NonSix(other)) => {
                let attempts = self.exec.note_failure(mint, now).await;
                warn!(%mint, decimals = other, attempts, "non-6-decimal mint; live sell refused (fail-closed)");
                return false;
            }
            Err(DecimalsRefusal::Unreadable(error)) => {
                let attempts = self.exec.note_failure(mint, now).await;
                warn!(%mint, %error, attempts, "mint decimals unreadable; live sell deferred (counts toward retry budget)");
                return false;
            }
        };
        let probe = {
            let Some(tracked) = self.exec.tracked(mint).await else {
                // Nothing tracked: settle only when no manual bag
                // remains either. A parked manual bag is still live
                // exposure, so this tick's paper close must defer like
                // any unsettled live hold (the caller stashes on
                // `false`) — feeding it as a win would hide real
                // losses behind paper that never exited.
                if !self.exec.manual_holds_base(mint).await {
                    self.flush_deferred_pnl(mint).await;
                    return true;
                }
                return false;
            };
            // Whole-token slice via the shared paper rule (floored,
            // never rounded): overselling is unauthorized and the live
            // leg must sell exactly the paper slice the book closed, or
            // the mirror drifts by dust forever.
            let held = tracked.amount_tokens;
            let sell_amount = simulator::slice_quantity(held, fraction_closed);
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
                mint: tracked.mint,
                curve: tracked.curve,
                assoc_curve: tracked.assoc_curve,
                amount_tokens: sell_amount,
                creator: tracked.creator,
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
        let attempts = self.exec.retry_attempts(mint).await;
        let tip = trader.effective_tip(attempts).await;
        // Never compete with our own inflight bundle: a resend while
        // the prior sell is still engine-pending risks two live
        // bundles for one bag. Both Pending and Unknown hold the
        // resend (status outage must not fork exits). Unconditional:
        // GiveUp resets the retry budget before parking, so a gated
        // `attempts > 0` check would go blind exactly when a stale
        // bundle may still land.
        if trader.prior_sell_pending(mint).await {
            // A hold is not free: a bundle stuck Pending/Unknown
            // forever would strand the exit with no budget trail.
            // Count holds like failures so the bag parks to manual
            // after the same budget instead of holding forever.
            let attempts = self.exec.note_failure(mint, now).await;
            if attempts >= SELL_MAX_ATTEMPTS {
                return self
                    .park_for_manual_exit(
                        trader,
                        mint,
                        now,
                        "prior sell bundle still inflight past the retry budget",
                    )
                    .await;
            }
            warn!(%mint, attempts, "prior sell bundle still inflight (Pending/Unknown); holding resend");
            return false;
        }
        // Venue tag for the dispatch record (journal), decided by the
        // same routing the dispatch itself uses.
        let venue_tag = match &pool {
            Some(leg) => match leg.venue {
                crate::live::PoolVenue::PumpSwap => "pumpswap",
                crate::live::PoolVenue::RaydiumV4 => "raydium",
            },
            None => "curve",
        };
        let sell_result = match &pool {
            Some(leg) => match leg.venue {
                crate::live::PoolVenue::PumpSwap => {
                    warn!(%mint, amm = %leg.amm, leg_mint = %leg.mint, "routing post-migration exit via PumpSwap");
                    trader.sell_pumpswap(&PumpSwapSellParams {
                        position: &probe,
                        pool: leg,
                        tip_lamports: tip,
                    }).await
                }
                crate::live::PoolVenue::RaydiumV4 => {
                    warn!(%mint, amm = %leg.amm, leg_mint = %leg.mint, "routing post-migration exit via Raydium");
                    trader.sell_raydium(&RaydiumSellParams {
                        position: &probe,
                        pool: leg,
                        tip_lamports: tip,
                    }).await
                }
            },
            None => {
                trader
                    .sell(&LiveSellParams {
                        position: &probe,
                        mark: *mark,
                        sol_usd,
                        venue_fee_bps: Some(venue_fee_bps),
                        tip_lamports: tip,
                        decimals,
                    })
                    .await
            }
        };
        match sell_result {
            Ok(bundle) => {
                // WAL first.
                trader.record_sell(mint, probe.amount_tokens, &bundle);
                let emptied = self.exec.subtract_fill(mint, probe.amount_tokens).await;
                // Success resets the budget.
                self.exec.reset_retry(mint).await;
                // Feed stashed paper PnL only when live holds nothing
                // anymore: a tracked remainder or a parked manual bag
                // (exact or suffixed dup) is still exposure. The tail
                // flushes at full clear instead.
                if emptied && !self.exec.manual_holds_base(mint).await {
                    self.flush_deferred_pnl(mint).await;
                }
                self.maybe_release_follow(mint).await;
                // Dispatch cost at the ACTUAL tip: the day-loss gate
                // sees live bleed, not paper-only profit.
                let tip_usd = Self::dispatch_usd(trader.dispatch_cost_for_tip(tip), sol_usd);
                self.debit_dispatch_usd(tip_usd).await;
                // Execution-gap calibration: pool legs settle at vault
                // prices, curve legs near this tick; the tick is the
                // shared proxy (see `FillGap` docs for what this omits).
                let gap = self
                    .exec
                    .note_gap_live_settle(mint, price_usd, tip_usd, venue_tag, mark.liquidity_usd)
                    .await;
                // Dispatch record: what was sent, at what, costing what.
                if let Some(j) = self.journal() {
                    j.append(&JournalLine::SellAttempt {
                        ts: now_secs(),
                        mint: mint.to_string(),
                        bundle: bundle.clone(),
                        venue: venue_tag.to_string(),
                        tokens: probe.amount_tokens,
                        price_usd,
                        debit_usd: Some(tip_usd),
                        // The paired Gap line (below, from the same
                        // tracker Option) already owns this tip; only
                        // an unpaired settle's tip is replayed as an
                        // explicit subtraction.
                        settle_paired: gap.is_some(),
                    });
                    if let Some(gap) = gap {
                        j.append(&JournalLine::Gap {
                            ts: now_secs(),
                            mint: gap.mint,
                            leg: "exit".to_string(),
                            paper_price_usd: gap.paper_price_usd,
                            realized_price_usd: gap.settle_price_usd,
                            qty_tokens: gap.qty_tokens as f64,
                            tip_usd: Some(gap.tip_usd),
                            gap_usd: gap.gap_usd,
                            venue: gap.venue,
                            liquidity_usd: gap.liquidity_usd,
                        });
                    }
                }
                warn!(%mint, %bundle, "LIVE sell dispatched via Jito (real funds)");
                self.emit(Alerter::format_live_sell(&self.card_ctx(), mint, &bundle));
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
                    let cause = if deterministic {
                        format!(
                            "deterministically rejected: {}",
                            ids::strip_error_url(&e.to_string())
                        )
                    } else {
                        "retries exhausted".to_string()
                    };
                    return self.park_for_manual_exit(trader, mint, now, &cause).await;
                } else if deterministic {
                    warn!(%mint, error = %ids::strip_error_url(&e.to_string()), attempts, "LIVE sell rejected before dispatch; holding, will re-probe");
                    false
                } else {
                    warn!(%mint, error = %ids::strip_error_url(&e.to_string()), "LIVE sell unconfirmed; holding, will retry");
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
            } else if let Some(curve) = self.exec.manual_curve(mint).await {
                Some(curve)
            } else {
                // Suffixed dup bags park under `{mint}#…`: no exact-key
                // bag, so price off the first parked dup (drained below).
                // Without any parked bag there is nothing to do.
                let mut found = None;
                for key in self.exec.manual_keys().await {
                    if key != mint && live_base_mint(&key) == mint {
                        found = self.exec.manual_curve(&key).await;
                        break;
                    }
                }
                found
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
        // duplicates are drained by the loop at the end of this call.
        if !self.exec.is_tracked(mint).await
            && let Some(held) = self.exec.take_manual_exact(mint).await
        {
            warn!(%mint, amount = held.amount_tokens, "manual sell resurrects a parked bag for explicit exit");
            if let Some(trader) = self.exec.live.as_ref() {
                trader.unrecord_manual(&held);
            }
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
            warn!(%mint, dups = ?parked_dups, "other parked bags for this mint drain after the primary settles");
        }
        // Feed boundary: the reserve feed keys on strings; custody holds
        // the validated key (converted once here, not re-parsed).
        let curve_str = curve.to_string();
        let tick = match self.reserves.fetch_tick(mint, &curve_str).await {
            Ok(tick) => tick,
            Err(CurveError::BadMintKey(_)) | Err(CurveError::BadCurveKey(_)) => {
                // Unroutable, but custody may still be held (tracked or
                // parked): dropping the request would orphan the bag
                // while telling the operator it settled. Keep retrying;
                // locks/tiers stay so the next round can proceed.
                warn!(%mint, "manual sell request unroutable; keeping request while custody is held");
                return false;
            }
            Err(e) => {
                warn!(%mint, error = %ids::strip_error_url(&e.to_string()), "manual sell price unreadable; keeping request for next round");
                return false;
            }
        };
        if now_secs().saturating_sub(tick.observed_at) >= CURVE_TICK_STALE_SECS {
            warn!(mint = %tick.mint, "stale curve tick — manual sell deferred to next round");
            return false;
        }
        // Same mark rule as the tick path (exact reserves while the
        // curve is the venue; a migrated one books the stale spot).
        let mark = crate::live::paper_mark_from_tick(&tick);
        let mut portfolio = self.portfolio.lock().await;
        let ids: Vec<u64> = portfolio.positions_for_mint(&tick.mint);
        let mut closed_pnls: Vec<f64> = Vec::new();
        let mut closed_cards: Vec<(String, f64, f64, f64, bool)> = Vec::new();
        // Gap notes flush after unlock (never nest the gaps lock inside
        // the portfolio guard; same discipline as the card emits below).
        let mut gap_notes: Vec<(f64, u64)> = Vec::new();
        let now = now_secs();
        // Decision-input record BEFORE the closes (manual-sell path):
        // each exit re-derives from exactly this state line, and the
        // `manual_exits` flag marks the trigger as the operator's call.
        if let Some(j) = self.journal() {
            j.append(&JournalLine::Tick {
                ts: now,
                mint: tick.mint.clone(),
                mark,
                sol_feed: self.sol_feed.price(),
                tick_observed_at: tick.observed_at,
                complete: false,
                manual_exits: true,
            });
        }
        for id in ids {
            match portfolio.close_position(id, &mark, ExitReason::Manual, tick.observed_at) {
                Ok(trade) => {
                    let headline = breaker_headline(trade.net_pnl, trade.dispatch_usd);
                    closed_pnls.push(headline);
                    gap_notes.push((trade.exit_price, trade.quantity));
                    self.journal_exit(&tick.mint, &trade, 1.0, &mark, tick.observed_at);
                    let mut marks: HashMap<String, simulator::Mark> = HashMap::new();
                    marks.insert(tick.mint.clone(), mark);
                    let equity = portfolio.snapshot(&marks).equity;
                    info!(mint = %tick.mint, id, reason = ?trade.reason,
                        net = format!("{:+.2}", headline), "paper position closed (manual sell)");
                    closed_cards.push((
                        format!("{:?}", trade.reason),
                        headline,
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
                &self.card_ctx(),
                &tick.mint, &reason, pnl, pct, equity, moonbag,
            ));
        }
        for (paper_price, qty) in gap_notes {
            self.exec.note_gap_paper_close(&tick.mint, paper_price, qty).await;
        }
        // Live first, breaker second: sell whatever is tracked
        // (primary, or a parked bag resurrected above), pulling parked
        // dups (`{mint}#2-…`, `#wal-…`) into the free slot one by one.
        // No paper legs pair with dups (paper holds at most one
        // position per mint). Paper PnL feeds the breaker only after
        // everything settles; tracked custody is never replaced.
        if self.exec.live.is_some() {
            loop {
                if !self.exec.is_tracked(&tick.mint).await {
                    let Some(dup_key) = self
                        .exec
                        .manual_keys()
                        .await
                        .into_iter()
                        .find(|k| k != mint && live_base_mint(k) == mint)
                    else {
                        break;
                    };
                    let Some(held) = self.exec.take_manual_exact(&dup_key).await
                    else {
                        break;
                    };
                    if let Some(trader) = self.exec.live.as_ref() {
                        trader.unrecord_manual(&held);
                    }
                    self.exec.insert_tracked(held).await;
                }
                if !self.live_sell(&tick.mint, &mark, 1.0).await
                {
                    warn!(mint = %tick.mint, "manual sell: live still holds; breaker PnL deferred, request kept");
                    self.exec.stash_deferred_pnl(&tick.mint, closed_pnls).await;
                    return false;
                }
            }
        }
        if self.exec.is_synthetic(&tick.mint).await {
            warn!(mint = %tick.mint, "synthetic paper PnL excluded from day-loss breaker (override price, no market)");
        } else {
            for pnl in closed_pnls {
                self.daily.lock().await.record(pnl, now);
            }
        }
        self.settle_close_tail(&tick).await;
        // Bags may remain (GiveUp-parked): keep the request so the
        // next `sell` round continues the drain instead of dropping a
        // file for custody that is still held.
        if self.exec.manual_holds_base(&tick.mint).await {
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::LivePosition;

    /// Deterministic custody key for a test name: production keys are
    /// base58 `Pubkey`s, so tests derive from the name instead of
    /// pretending arbitrary strings are addresses.
    fn key_of(name: &str) -> (Pubkey, String) {
        let b = name.bytes().fold(0u8, |a, x| a.wrapping_add(x));
        let key = Pubkey::new_from_array([b; 32]);
        let s = key.to_string();
        (key, s)
    }

    fn bag(mint: &str, amount: u64) -> LivePosition {
        let (mint_key, _) = key_of(mint);
        let (curve_key, _) = key_of(&format!("{mint}-curve"));
        let (assoc_key, _) = key_of(&format!("{mint}-assoc"));
        LivePosition {
            mint: mint_key,
            curve: curve_key,
            assoc_curve: assoc_key,
            amount_tokens: amount,
            creator: None,
        }
    }

    fn exec() -> LiveExecution<LiveTrader> {
        LiveExecution::paper().with_live(None)
    }

    #[test]
    fn breaker_headline_nets_dispatch_bleed() {
        assert_eq!(breaker_headline(10.0, 0.5), 9.5);
        assert_eq!(breaker_headline(-3.0, 0.0), -3.0);
        // Non-finite dispatch never poisons the gate.
        assert_eq!(breaker_headline(10.0, f64::NAN), 10.0);
        assert_eq!(breaker_headline(10.0, f64::INFINITY), 10.0);
    }

    #[test]
    fn fill_gap_tracker_matches_paper_to_live_settle() {
        let mut t = FillGapTracker::new();
        assert_eq!(t.stats(), (0, 0.0, 0.0));
        // Unmatched settle records nothing (orphan legs, never fabricated).
        assert!(t.note_live_settle("M", 1.0, 0.05, "curve", 50_000.0).is_none());
        assert_eq!(t.stats(), (0, 0.0, 0.0));
        // Degenerate paper notes are ignored.
        t.note_paper_close("M", f64::NAN, 10);
        t.note_paper_close("M", 0.0, 10);
        t.note_paper_close("M", 1.0, 0);
        assert!(t.note_live_settle("M", 1.0, 0.05, "curve", 50_000.0).is_none());
        // Paper 2.0 x 10 tokens, live settles 1.9 + 0.05 tip:
        // (1.9 - 2.0) * 10 - 0.05 = -1.05.
        t.note_paper_close("M", 2.0, 10);
        let gap = t
            .note_live_settle("M", 1.9, 0.05, "curve", 50_000.0)
            .expect("gap");
        assert_eq!(gap.qty_tokens, 10);
        assert!((gap.gap_usd + 1.05).abs() < 1e-9, "gap={}", gap.gap_usd);
        // Consumed: a second settle without a new paper note is orphan.
        assert!(t.note_live_settle("M", 1.9, 0.05, "curve", 50_000.0).is_none());
        let (n, avg, total) = t.stats();
        assert_eq!(n, 1);
        assert!((avg + 1.05).abs() < 1e-9);
        assert!((total + 1.05).abs() < 1e-9);
        // Non-finite settle keeps the pending note for the next round.
        t.note_paper_close("N", 3.0, 5);
        assert!(t.note_live_settle("N", f64::NAN, 0.05, "curve", 50_000.0).is_none());
        let gap = t
            .note_live_settle("N", 3.0, 0.05, "curve", 50_000.0)
            .expect("kept");
        assert!((gap.gap_usd + 0.05).abs() < 1e-9);
    }

    #[test]
    fn fill_gap_tracker_sheds_oldest_past_cap() {
        let mut t = FillGapTracker::new();
        for _ in 0..(FILL_GAP_CAP + 10) {
            t.note_paper_close("M", 1.0, 1);
            t.note_live_settle("M", 1.0, 0.0, "curve", 50_000.0)
                .expect("gap");
        }
        // Bounded history; totals still accumulate (windowed avg, full sum).
        assert_eq!(t.gaps.len(), FILL_GAP_CAP);
        let (n, _, _) = t.stats();
        assert_eq!(n, FILL_GAP_CAP);
    }

    #[test]
    fn entry_gap_records_landed_fill() {
        let mut t = FillGapTracker::new();
        assert_eq!(t.entry_stats(), (0, 0.0, 0.0));
        // Degenerate inputs record nothing.
        assert!(t.note_entry_gap("M", f64::NAN, 1.0, 10.0, "curve", 50_000.0, 1_000).is_none());
        assert!(t.note_entry_gap("M", 1.0, 0.0, 10.0, "curve", 50_000.0, 1_000).is_none());
        assert!(t.note_entry_gap("M", 1.0, 1.0, 0.0, "curve", 50_000.0, 1_000).is_none());
        assert_eq!(t.entry_stats(), (0, 0.0, 0.0));
        // Paper 0.0000042, landed 0.0000045 over 2.35M tokens: live
        // paid more, so the gap is negative like the exit leg
        // ((0.0000042 - 0.0000045) * 2_350_000 = -0.705).
        let gap = t
            .note_entry_gap(
                "M",
                0.000_004_2,
                0.000_004_5,
                2_350_000.0,
                "curve",
                50_000.0,
                1_000,
            )
            .expect("gap");
        assert!((gap.gap_usd + 0.705).abs() < 1e-9, "gap={}", gap.gap_usd);
        let (n, avg, total) = t.entry_stats();
        assert_eq!(n, 1);
        assert!((avg + 0.705).abs() < 1e-9);
        assert!((total + 0.705).abs() < 1e-9);
    }

    #[test]
    fn fill_gap_tracker_settles_fifo_per_mint() {
        // Moonbag shape: 0.8 scale-out then runner close. Each settle
        // consumes the oldest unmatched note — no leg goes orphan.
        let mut t = FillGapTracker::new();
        t.note_paper_close("M", 2.0, 8);
        t.note_paper_close("M", 3.0, 2);
        let first = t
            .note_live_settle("M", 1.9, 0.0, "curve", 50_000.0)
            .expect("first leg");
        assert_eq!((first.paper_price_usd, first.qty_tokens), (2.0, 8));
        let second = t
            .note_live_settle("M", 2.9, 0.0, "curve", 50_000.0)
            .expect("second leg");
        assert_eq!((second.paper_price_usd, second.qty_tokens), (3.0, 2));
        assert!(t
            .note_live_settle("M", 2.9, 0.0, "curve", 50_000.0)
            .is_none());
        let (n, _, _) = t.stats();
        assert_eq!(n, 2);
    }

    #[test]
    fn venue_stats_split_exit_and_entry() {
        let mut t = FillGapTracker::new();
        // Two curve exits at -1.0 each, one pumpswap exit at -5.0.
        for _ in 0..2 {
            t.note_paper_close("C", 2.0, 10);
            t.note_live_settle("C", 1.9, 0.0, "curve", 50_000.0)
                .expect("curve gap");
        }
        t.note_paper_close("P", 2.0, 10);
        t.note_live_settle("P", 1.5, 0.0, "pumpswap", 5_000.0)
            .expect("pool gap");
        let exits = t.venue_exit_stats();
        assert_eq!(exits.len(), 2);
        assert_eq!(exits[0].0, "curve");
        assert_eq!(exits[0].1, 2);
        assert!((exits[0].2 + 1.0).abs() < 1e-9, "avg={}", exits[0].2);
        assert_eq!(exits[1].0, "pumpswap");
        assert!((exits[1].3 + 5.0).abs() < 1e-9, "total={}", exits[1].3);
        // Entries aggregate separately (buys are structurally curve).
        t.note_entry_gap("E", 1.0, 1.1, 10.0, "curve", 50_000.0, 1_000)
            .expect("entry gap");
        let entries = t.venue_entry_stats();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "curve");
        assert!((entries[0].3 + 1.0).abs() < 1e-9, "total={}", entries[0].3);
    }

    #[test]
    fn worst_cell_needs_min_samples() {
        let mut t = FillGapTracker::new();
        // Costly cell: $20 ticket on a $500-liq curve, gap -2.0 each.
        // notional = 10 tokens × $2 = $20 → size bucket 1;
        // $500 liq → liq bucket 0.
        for i in 0..2 {
            t.note_paper_close(&format!("M{i}"), 2.0, 10);
            t.note_live_settle(&format!("M{i}"), 1.8, 0.0, "curve", 500.0)
                .expect("gap");
        }
        // Thin cell must not win: unknown reads as unknown, never as
        // a verdict the operator (or a gate) could act on.
        assert!(t.worst_exit_cell().is_none());
        t.note_paper_close("M2", 2.0, 10);
        t.note_live_settle("M2", 1.8, 0.0, "curve", 500.0)
            .expect("gap");
        let (venue, size, liq, cell) = t.worst_exit_cell().expect("cell");
        assert_eq!(venue, "curve");
        assert_eq!((size, liq), (1, 0));
        assert_eq!(cell.n, 3);
        assert!((cell.avg_gap_usd + 2.0).abs() < 1e-9, "avg={}", cell.avg_gap_usd);
        // Entry window has no samples yet: no twin verdict either.
        assert!(t.worst_entry_cell().is_none());
    }

    #[test]
    fn expected_entry_gap_unknown_until_proven() {
        let mut t = FillGapTracker::new();
        // $20 ticket (10 × $2 paper) on $50k liq, gap -0.5 each.
        for _ in 0..2 {
            t.note_entry_gap("M", 2.0, 2.05, 10.0, "curve", 50_000.0, 1_000)
                .expect("gap");
        }
        assert!(t.expected_entry_gap_usd("curve", 20.0, 50_000.0).is_none());
        t.note_entry_gap("M", 2.0, 2.05, 10.0, "curve", 50_000.0, 1_000)
            .expect("gap");
        let exp = t
            .expected_entry_gap_usd("curve", 20.0, 50_000.0)
            .expect("proven");
        assert!((exp + 0.5).abs() < 1e-9, "exp={exp}");
        // A whale ticket on the same curve is a different cell: unknown.
        assert!(t.expected_entry_gap_usd("curve", 200.0, 50_000.0).is_none());
        // Same shape on a venue that never filled: unknown.
        assert!(
            t.expected_entry_gap_usd("raydium", 20.0, 50_000.0)
                .is_none()
        );
    }

    #[test]
    fn entry_hour_stats_bucket_utc() {
        let mut t = FillGapTracker::new();
        t.note_entry_gap("A", 1.0, 1.1, 10.0, "curve", 50_000.0, 0)
            .expect("gap");
        t.note_entry_gap("B", 1.0, 1.2, 10.0, "curve", 50_000.0, 5 * 3_600)
            .expect("gap");
        t.note_entry_gap("C", 1.0, 1.3, 10.0, "curve", 50_000.0, 5 * 3_600 + 59)
            .expect("gap");
        let hours = t.entry_hour_stats();
        assert_eq!(hours[0].0, 1);
        assert!((hours[0].1 + 1.0).abs() < 1e-9, "avg={}", hours[0].1);
        assert_eq!(hours[5].0, 2);
        assert!((hours[5].2 + 5.0).abs() < 1e-9, "total={}", hours[5].2);
        assert_eq!(hours[6].0, 0);
        assert_eq!((hours[6].1, hours[6].2), (0.0, 0.0));
    }

    #[test]
    fn gap_buckets_follow_documented_edges() {
        // Size: [10, 25, 50, 100] → 5 buckets; liq: [1k, 10k, 100k] → 4.
        assert_eq!((size_bucket(9.99), size_bucket(10.0)), (0, 1));
        assert_eq!((size_bucket(99.99), size_bucket(100.0)), (3, 4));
        assert_eq!(size_bucket(10_000.0), 4);
        assert_eq!((liq_bucket(999.0), liq_bucket(1_000.0)), (0, 1));
        assert_eq!(liq_bucket(1_000_000.0), 3);
        // Non-finite/negative reads bucket 0 (drained/unknown earns no
        // precision; downstream fail-closed is owned elsewhere).
        assert_eq!(size_bucket(f64::NAN), 0);
        assert_eq!(liq_bucket(-1.0), 0);
        // UTC hour wraps at midnight.
        assert_eq!(hour_of(0), 0);
        assert_eq!(hour_of(24 * 3_600 + 3_600), 1);
    }

    #[tokio::test]
    async fn tracked_lifecycle_insert_subtract_remove() {
        let exec = exec();
        let m = key_of("MINT").1;
        assert!(!exec.is_tracked(&m).await);
        exec.insert_tracked(bag("MINT", 1_000)).await;
        assert!(exec.is_tracked(&m).await);
        assert_eq!(exec.tracked(&m).await.expect("bag").amount_tokens, 1_000);
        assert!(!exec.subtract_fill(&m, 400).await);
        assert_eq!(exec.tracked(&m).await.expect("bag").amount_tokens, 600);
        assert!(exec.subtract_fill(&m, 600).await);
        assert!(!exec.is_tracked(&m).await);
        assert!(exec.subtract_fill(&m, 1).await);
        assert!(exec.remove_tracked(&m).await.is_none());
        // Over-subtract refuses instead of masking: the bag stays with
        // its held amount for investigation.
        exec.insert_tracked(bag("MINT", 100)).await;
        assert!(!exec.subtract_fill(&m, 101).await);
        assert_eq!(exec.tracked(&m).await.expect("bag").amount_tokens, 100);
    }

    #[tokio::test]
    async fn manual_park_resurrect_keeps_duplicates() {
        let exec = exec();
        let m = key_of("MINT").1;
        let dup = format!("{m}#2-abcdef12");
        exec.park_manual(m.clone(), bag("MINT", 100)).await;
        exec.park_manual(dup.clone(), bag("MINT", 50)).await;
        assert!(exec.manual_holds(&m).await);
        assert!(exec.manual_holds_base(&m).await);
        assert!(!exec.manual_holds("OTHER").await);
        assert_eq!(exec.manual_curve(&m).await, Some(key_of("MINT-curve").0));
        let held = exec.take_manual_exact(&m).await.expect("bag");
        assert_eq!(held.amount_tokens, 100);
        assert!(!exec.manual_holds(&m).await);
        assert!(exec.manual_holds_base(&m).await);
        assert_eq!(exec.manual_keys().await, vec![dup]);
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
        let (a, b) = (key_of("A").1, key_of("B").1);
        exec.insert_tracked(bag("B", 10)).await;
        exec.insert_tracked(bag("A", 10)).await;
        exec.park_manual(format!("{a}#2-abcdef12"), bag("A", 5)).await;
        exec.park_manual(format!("{}#late-12345678", key_of("C").1), bag("C", 5)).await;
        let exposure = exec
            .exposure_mints(vec![key_of("C").1, "D".to_string()])
            .await;
        let mut want = vec![a, b, key_of("C").1, "D".to_string()];
        want.sort();
        assert_eq!(exposure, want);
    }

    #[tokio::test]
    async fn tracked_and_manual_key_listings() {
        let exec = exec();
        let m = key_of("MINT").1;
        assert!(exec.tracked_keys().await.is_empty());
        exec.insert_tracked(bag("MINT", 1)).await;
        assert_eq!(exec.tracked_keys().await, vec![m.clone()]);
        assert!(exec.remove_tracked(&m).await.is_some());
        assert!(exec.tracked_keys().await.is_empty());
    }
}

//! Post-migration watch: graduation cards, pool follows, spike alerts.
//!
//! Moved verbatim from `pipeline.rs` (same `impl Pipeline` block, new
//! home). Behavior unchanged.

use alert::Alerter;
use listener::MigrationReport;
use migration::PoolTick;
use tracing::{info, warn};

use crate::pipeline::{AlertSink, Auditor, LiveSink, Pipeline, ReserveFeed, Screener};
use crate::settings::now_secs;

/// Post-migration spike multiple that fires the follow alert once.
pub const FOLLOW_SPIKE_MULT: f64 = 2.0;

/// Pool ticks older than this (seconds) are ignored: a stale baseline or
/// spike check is worse than none (thin/manipulated prints linger).
pub const POOL_TICK_STALE_SECS: u64 = 300;

/// Post-migration follow state per mint.
#[derive(Debug, Clone)]
pub struct FollowState {
    /// First pool-tick price (baseline).
    pub entry_price: f64,
    /// Whether the spike card already fired.
    pub alerted_spike: bool,
    /// Last pool-tick time (unix secs): idle follows older than
    /// [`FOLLOW_TTL_SECS`] are evicted (dead pools, bounded growth).
    pub last_seen_secs: u64,
}

/// Idle-follow TTL: post-migration pools that never spike stop being
/// polled after a day (evicted with their vault watch).
pub const FOLLOW_TTL_SECS: u64 = 86_400;
/// Reported-migration memory: entries older than a day are pruned (a
/// stale entry only risks a duplicate card, never a miss).
pub const MIGRATED_TTL_SECS: u64 = 86_400;
/// Backstop cap for reported-migration memory (oldest-first eviction).
pub const MIGRATED_CAP: usize = 5_000;

impl<A: Auditor, R: ReserveFeed, L: LiveSink, S: Screener, T: AlertSink> Pipeline<A, R, L, S, T> {

    /// Hunt and announce the migration exactly once per mint. Runs after
    /// positions close: classification plus pool addresses for instant
    /// Raydium routing follow-ups.
    pub(crate) async fn report_migration_once(&self, mint: &str, curve: &str) {
        {
            let mut migrated = self.migrated.lock().await;
            let now = now_secs();
            if migrated.insert(mint.to_string(), now).is_some() {
                return;
            }
            // Bounded growth: graduated mints never un-graduate, so
            // pruning day-old entries only risks a duplicate card.
            migrated.retain(|_, at| now.saturating_sub(*at) <= MIGRATED_TTL_SECS);
            if migrated.len() > MIGRATED_CAP
                && let Some(oldest) = migrated
                    .iter()
                    .min_by_key(|(_, at)| **at)
                    .map(|(mint, _)| mint.clone())
            {
                migrated.remove(&oldest);
            }
        }
        match self.screener.detect_migration(curve, mint).await {
            Ok(Some(report)) => {
                info!(mint = %mint, sig = %report.signature, dest = ?report.destination, amm = ?report.amm_id, "migration detected");
                self.emit(Alerter::format_migration(
                    mint,
                    &format!("{:?}", report.destination),
                    &report.signature,
                    report.amm_id.as_deref(),
                    report.base_mint.as_deref(),
                    report.quote_mint.as_deref(),
                ));
                self.follow_pool(mint, &report).await;
            }
            Ok(None) => {
                info!(mint = %mint, "graduated but no migration tx found yet; will retry on later ticks");
                // Allow a later tick to retry the hunt (the migration tx
                // may not be indexed yet right after graduation).
                self.migrated.lock().await.remove(mint);
            }
            Err(e) => {
                warn!(%mint, error = %e, "migration hunt failed");
                // Allow a later tick to retry the hunt.
                self.migrated.lock().await.remove(mint);
            }
        }
    }

    /// Register a migrated Raydium pool for post-migration price tracking.
    /// PumpSwap migrations carry no pool: nothing to follow.
    /// Raydium follows are price-only (spike cards, never swaps — no
    /// Raydium swap builder yet).
    async fn follow_pool(&self, mint: &str, report: &MigrationReport) {
        let (Some(amm), Some(base_vault), Some(quote_vault)) = (
            report.amm_id.clone(),
            report.base_vault.clone(),
            report.quote_vault.clone(),
        ) else {
            // Pool unresolvable (trust-gate failure or PumpSwap): loud, so
            // a missing follow is never mistaken for a quiet market.
            warn!(mint = %mint, amm = ?report.amm_id, "migration without resolvable pool; not following");
            return;
        };
        if let Err(e) = self
            .vaults
            .watch_defaults(mint, &amm, &base_vault, &quote_vault)
            .await
        {
            warn!(amm = %amm, error = %e, "pool watch failed");
            return;
        }
        // Seed the follow entry NOW (baseline set on first valid tick):
        // pools that never tick would otherwise stay watched forever —
        // the idle sweep below only sees `followed`, and a dead pool
        // burns 2 vault RPCs per round with no expiry. `or_insert` keeps
        // a baseline a racing tick already seeded.
        self.followed
            .lock()
            .await
            .entry(mint.to_string())
            .or_insert(FollowState {
                entry_price: f64::NAN,
                alerted_spike: false,
                last_seen_secs: now_secs(),
            });
        info!(%mint, amm = %amm, "following migrated pool");
    }

    /// One post-migration pool tick: seed the entry on first sight, then
    /// alert once the price doubles off it. Paper tracking only (price
    /// cards, never swaps — PumpSwap carries no pool, and Raydium has no
    /// swap builder yet); entries need a Raydium swap builder (future work).
    pub async fn on_pool_tick(&self, tick: &PoolTick) {
        // Validate at the boundary: non-finite/non-positive prices must not
        // seed baselines or trigger alerts; stale ticks are dropped.
        if !tick.price_usd.is_finite() || tick.price_usd <= 0.0 {
            warn!(mint = %tick.mint, price = tick.price_usd, "ignoring degenerate pool tick");
            return;
        }
        if now_secs().saturating_sub(tick.observed_at) > POOL_TICK_STALE_SECS {
            warn!(mint = %tick.mint, observed_at = tick.observed_at, "ignoring stale pool tick");
            return;
        }
        let now = now_secs();
        // Evict idle follows first (dead pools): collect under the guard,
        // unwatch outside it — vaults own a separate lock that must never
        // be acquired while `followed` is held.
        let stale: Vec<String> = {
            let followed = self.followed.lock().await;
            followed
                .iter()
                .filter(|(_, state)| now.saturating_sub(state.last_seen_secs) > FOLLOW_TTL_SECS)
                .map(|(mint, _)| mint.clone())
                .collect()
        };
        for mint in &stale {
            self.vaults.unwatch(mint).await;
        }
        if !stale.is_empty() {
            let mut followed = self.followed.lock().await;
            for mint in &stale {
                followed.remove(mint);
            }
        }
        let mut followed = self.followed.lock().await;
        let state = followed.entry(tick.mint.clone()).or_insert(FollowState {
            entry_price: tick.price_usd,
            alerted_spike: false,
            last_seen_secs: now,
        });
        state.last_seen_secs = now;
        if state.alerted_spike {
            return;
        }
        // Baseline for follows seeded without a first tick (see
        // `follow_pool`): the first valid print sets it, never alerts.
        if state.entry_price.is_nan() {
            state.entry_price = tick.price_usd;
            return;
        }
        if tick.price_usd >= state.entry_price * FOLLOW_SPIKE_MULT {
            state.alerted_spike = true;
            let mint = tick.mint.clone();
            drop(followed);
            info!(mint = %mint, multiple = FOLLOW_SPIKE_MULT, price = tick.price_usd, "post-migration spike");
            self.emit(Alerter::format_follow_spike(
                &mint,
                FOLLOW_SPIKE_MULT,
                tick.price_usd,
            ));
            // Alert fired: release the pool watch and the follow entry so a
            // dead pool is not polled forever (unbounded growth).
            self.vaults.unwatch(&mint).await;
            self.followed.lock().await.remove(&mint);
        }
    }
}

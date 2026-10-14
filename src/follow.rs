//! Post-migration watch: cards, pool follows, spike alerts.

use alert::Alerter;
use listener::MigrationReport;
use migration::PoolTick;
use tracing::{info, warn};

use crate::pipeline::{AlertSink, Auditor, LiveSink, Pipeline, ReserveFeed, Screener};
use crate::settings::now_secs;

/// Spike multiple firing the follow alert once.
pub const FOLLOW_SPIKE_MULT: f64 = 2.0;

/// Old pool ticks are ignored.
pub const POOL_TICK_STALE_SECS: u64 = 300;

/// Pool watch entry with exit leg. Dies on idle sweep.
#[derive(Debug, Clone)]
pub struct FollowState {
    /// First pool-tick price.
    pub entry_price: f64,
    /// Spike card fired.
    pub alerted_spike: bool,
    /// Last tick time (unix secs).
    pub last_seen_secs: u64,
    /// Raydium leg (`None` when unresolvable).
    pub pool: Option<crate::live::RaydiumPoolLeg>,
}

/// Idle-follow TTL (a day).
pub const FOLLOW_TTL_SECS: u64 = 86_400;
/// Migration memory TTL (a day).
pub const MIGRATED_TTL_SECS: u64 = 86_400;
/// Cap for migration memory (oldest evicted first).
pub const MIGRATED_CAP: usize = 5_000;

impl<A: Auditor, R: ReserveFeed, L: LiveSink, S: Screener, T: AlertSink> Pipeline<A, R, L, S, T> {

    /// Announce each migration once.
    pub(crate) async fn report_migration_once(&self, mint: &str, curve: &str) {
        {
            let mut migrated = self.migrated.lock().await;
            let now = now_secs();
            if migrated.insert(mint.to_string(), now).is_some() {
                return;
            }
            // Bounded growth: pruning only risks a dup card.
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
                // Retry later (tx may not be indexed yet).
                self.migrated.lock().await.remove(mint);
            }
            Err(e) => {
                warn!(%mint, error = %e, "migration hunt failed");
                // Retry later.
                self.migrated.lock().await.remove(mint);
            }
        }
    }

    /// Register a migrated pool for tracking and exits.
    /// PumpSwap has no pool: nothing to follow.
    async fn follow_pool(&self, mint: &str, report: &MigrationReport) {
        let (Some(amm), Some(base_vault), Some(quote_vault)) = (
            report.amm_id.clone(),
            report.base_vault.clone(),
            report.quote_vault.clone(),
        ) else {
            // Loud, so a missing follow is never silent.
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
        // Seed now: pools that never tick still expire. Keeps raced baseline.
        // A tick-first entry carries pool None: overwrite the leg when
        // the hunt resolves it, or routing stays blind until eviction.
        let pool = Some(crate::live::RaydiumPoolLeg {
            mint: mint.to_string(),
            amm: amm.clone(),
            base_vault: base_vault.clone(),
            quote_vault: quote_vault.clone(),
        });
        self.followed
            .lock()
            .await
            .entry(mint.to_string())
            .and_modify(|state| {
                state.pool = pool.clone();
                state.last_seen_secs = now_secs();
            })
            .or_insert(FollowState {
                entry_price: f64::NAN,
                alerted_spike: false,
                last_seen_secs: now_secs(),
                pool,
            });
        info!(%mint, amm = %amm, "following migrated pool");
    }

    /// Sweep idle follows even when no pool ticks arrive (call from the
    /// poll loop on a timer). `on_pool_tick` also sweeps, but a fully
    /// idle ticker would otherwise leak `followed` + vault subs to restart.
    pub async fn sweep_idle(&self) -> Vec<String> {
        let now = now_secs();
        let candidates: Vec<String> = {
            let followed = self.followed.lock().await;
            followed
                .iter()
                .filter(|(_, state)| now.saturating_sub(state.last_seen_secs) > FOLLOW_TTL_SECS)
                .map(|(mint, _)| mint.clone())
                .collect()
        };
        let mut stale = Vec::new();
        for mint in candidates {
            let held = match &self.exec.live {
                Some(trader) => self.is_live_tracked(trader, &mint).await,
                None => false,
            };
            if !held {
                stale.push(mint);
            }
        }
        for mint in &stale {
            self.vaults.unwatch(mint).await;
        }
        if !stale.is_empty() {
            let mut followed = self.followed.lock().await;
            for mint in &stale {
                followed.remove(mint);
            }
        }
        stale
    }

    /// One pool tick: seed baseline, alert once on 2x.
    pub async fn on_pool_tick(&self, tick: &PoolTick) {
        // Bad ticks never seed or alert.
        if !tick.price_usd.is_finite() || tick.price_usd <= 0.0 {
            warn!(mint = %tick.mint, price = tick.price_usd, "ignoring degenerate pool tick");
            return;
        }
        if now_secs().saturating_sub(tick.observed_at) > POOL_TICK_STALE_SECS {
            warn!(mint = %tick.mint, observed_at = tick.observed_at, "ignoring stale pool tick");
            return;
        }
        let now = now_secs();
        // Evict idle follows first. Candidates collected under a short
        // guard; custody checks await after it drops (never hold a lock
        // across await). Skips mints with live custody: evicting a held
        // bag's leg strands its Raydium exit to manual.
        let candidates: Vec<String> = {
            let followed = self.followed.lock().await;
            followed
                .iter()
                .filter(|(_, state)| now.saturating_sub(state.last_seen_secs) > FOLLOW_TTL_SECS)
                .map(|(mint, _)| mint.clone())
                .collect()
        };
        let mut stale = Vec::new();
        for mint in candidates {
            // Same custody rule as the spike path: any tracked, parked,
            // or pending bag keeps its leg.
            let held = match &self.exec.live {
                Some(trader) => self.is_live_tracked(trader, &mint).await,
                None => false,
            };
            if !held {
                stale.push(mint);
            }
        }
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
            // Ticks never create pool legs.
            pool: None,
        });
        state.last_seen_secs = now;
        if state.alerted_spike {
            return;
        }
        // First print seeds baseline, never alerts.
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
            // Release unless live still needs the leg.
            let keep_for_exit = match &self.exec.live {
                Some(trader) => self.is_live_tracked(trader, &mint).await,
                None => false,
            };
            if keep_for_exit {
                info!(mint = %mint, "live custody holds the pool leg past the spike; follow retained for Raydium exits");
            } else {
                self.vaults.unwatch(&mint).await;
                self.followed.lock().await.remove(&mint);
            }
        }
    }

    /// Release a post-spike follow once custody cleared.
    pub(crate) async fn maybe_release_follow(&self, mint: &str) {
        // Decide after the guard drops.
        let alerted = self
            .followed
            .lock()
            .await
            .get(mint)
            .is_some_and(|state| state.alerted_spike);
        if !alerted {
            return;
        }
        let reap = match &self.exec.live {
            Some(trader) => !self.is_live_tracked(trader, mint).await,
            None => true,
        };
        if reap {
            self.vaults.unwatch(mint).await;
            self.followed.lock().await.remove(mint);
            info!(mint = %mint, "post-spike follow released with settled custody");
        }
    }
}

//! Post-migration watch: cards, pool follows, spike alerts.

use alert::Alerter;
use listener::MigrationReport;
use migration::{MigrationDestination, PoolTick};
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
    /// Lifecycle stage (baseline seeding → watching → alerted).
    pub stage: FollowStage,
    /// Last tick time (unix secs).
    pub last_seen_secs: u64,
    /// Pool leg (`None` when unresolvable).
    pub pool: Option<crate::live::PoolLeg>,
}

/// Follow lifecycle. Replaces the old NaN-price + alerted-bool pair:
/// two encodings for one fact, and NaN silently poisons every
/// comparison it touches.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FollowStage {
    /// No baseline yet; the first tick seeds it (never alerts).
    Seeding,
    /// Baseline set; the 2x card fires at `entry_price * MULT`.
    Watching { entry_price: f64 },
    /// Spike card fired; the leg is retained only for live exits.
    Alerted,
}

/// Idle-follow TTL (a day).
pub const FOLLOW_TTL_SECS: u64 = 86_400;
/// Migration memory TTL (a day).
pub const MIGRATED_TTL_SECS: u64 = 86_400;
/// Cap for migration memory (oldest evicted first).
pub const MIGRATED_CAP: usize = 5_000;

impl<A: Auditor, R: ReserveFeed, L: LiveSink, S: Screener, T: AlertSink> Pipeline<A, R, L, S, T> {
    /// Announce each migration once. Owned parameters: the background
    /// custody cycle spawns this chain off the event loop, so
    /// its future must be a fully owned, provably `Send` state machine.
    pub(crate) async fn report_migration_once(&self, mint: String, curve: String) {
        {
            let mut migrated = self.migrated.lock().await;
            let now = now_secs();
            // Prune first: an expired entry must not swallow the retry.
            // Bounded growth: pruning only risks a dup card.
            migrated.retain(|_, at| now.saturating_sub(*at) <= MIGRATED_TTL_SECS);
            if migrated.insert(mint.clone(), now).is_some() {
                return;
            }
            if migrated.len() > MIGRATED_CAP
                && let Some(oldest) = migrated
                    .iter()
                    .min_by_key(|(_, at)| **at)
                    .map(|(mint, _)| mint.clone())
            {
                migrated.remove(&oldest);
            }
        }
        match self.screener.detect_migration(&curve, &mint).await {
            Ok(Some(report)) => {
                info!(mint = %mint, sig = %report.signature, dest = ?report.destination, amm = ?report.amm_id, "migration detected");
                self.emit(Alerter::format_migration(
                    &self.card_ctx(),
                    &mint,
                    &format!("{:?}", report.destination),
                    &report.signature,
                    report.amm_id.as_deref(),
                    report.base_mint.as_deref(),
                    report.quote_mint.as_deref(),
                ));
                // Retry contract: the dedupe entry is only a claim while
                // a pool leg exists. Registering failed (missing or
                // unwatchable legs) must re-arm the supervision retry
                // instead of leaving the bag permanently un-followed.
                if !self.follow_pool(&mint, &report).await {
                    self.migrated.lock().await.remove(&mint);
                }
            }
            Ok(None) => {
                info!(mint = %mint, "graduated but no migration tx found yet; will retry on later ticks");
                // Retry later (tx may not be indexed yet).
                self.migrated.lock().await.remove(&mint);
            }
            Err(e) => {
                warn!(%mint, error = %ids::strip_error_url(&e.to_string()), "migration hunt failed");
                // Retry later.
                self.migrated.lock().await.remove(&mint);
            }
        }
    }

    /// Register a migrated pool for tracking and exits.
    /// PumpSwap legs watch the pool account too (effective-quote
    /// pricing); on both venues BASE-side is WSOL by report contract.
    async fn follow_pool(&self, mint: &str, report: &MigrationReport) -> bool {
        let (Some(amm), Some(base_vault), Some(quote_vault)) = (
            report.amm_id.clone(),
            report.base_vault.clone(),
            report.quote_vault.clone(),
        ) else {
            // Loud, so a missing follow is never silent.
            warn!(mint = %mint, amm = ?report.amm_id, "migration without resolvable pool; not following");
            return false;
        };
        let watched = if report.destination == MigrationDestination::PumpSwap {
            // amm_id IS the pool on this venue.
            self.vaults
                .watch_pumpswap_defaults(mint, &amm, &amm, &base_vault, &quote_vault)
                .await
        } else {
            self.vaults
                .watch_defaults(mint, &amm, &base_vault, &quote_vault)
                .await
        };
        if let Err(e) = watched {
            warn!(amm = %amm, error = %ids::strip_error_url(&e.to_string()), "pool watch failed");
            return false;
        }
        // Seed now: pools that never tick still expire. Keeps raced baseline.
        // A tick-first entry carries pool None: overwrite the leg when
        // the hunt resolves it, or routing stays blind until eviction.
        let venue = match report.destination {
            MigrationDestination::PumpSwap => crate::live::PoolVenue::PumpSwap,
            MigrationDestination::RaydiumV4 => crate::live::PoolVenue::RaydiumV4,
        };
        // Follow boundary: parse once, fail closed. Downstream routing
        // never re-validates these legs.
        let parse = |label: &str, v: &str| match executor::parse_pubkey(v) {
            Ok(p) => Some(p),
            Err(e) => {
                warn!(%mint, %label, error = %e, "unparseable pool leg; not following (fail-closed)");
                None
            }
        };
        let (Some(mint_key), Some(amm_key), Some(base_key), Some(quote_key)) = (
            parse("mint", mint),
            parse("amm", &amm),
            parse("base_vault", &base_vault),
            parse("quote_vault", &quote_vault),
        ) else {
            return false;
        };
        let pool = Some(crate::live::PoolLeg {
            mint: mint_key,
            amm: amm_key,
            base_vault: base_key,
            quote_vault: quote_key,
            venue,
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
                stage: FollowStage::Seeding,
                last_seen_secs: now_secs(),
                pool,
            });
        info!(%mint, amm = %amm, "following migrated pool");
        true
    }

    /// Live custody (tracked or parked) that still rides a
    /// curve with no registered pool leg. After a restart the paper book
    /// is empty and a graduated curve may stop ticking (zero reserves),
    /// so these bags have no guaranteed automated exit feed. Deduped by
    /// mint, sorted for stable logs/tests.
    pub async fn bags_without_pool_leg(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        {
            let guard = self.exec.live_positions.lock().await;
            for pos in guard.values() {
                let mint = pos.mint.to_string();
                if seen.insert(mint.clone()) {
                    out.push((mint, pos.curve.to_string()));
                }
            }
        }
        {
            let guard = self.exec.live_manual.lock().await;
            for pos in guard.values() {
                let mint = pos.mint.to_string();
                if seen.insert(mint.clone()) {
                    out.push((mint, pos.curve.to_string()));
                }
            }
        }
        {
            let followed = self.followed.lock().await;
            out.retain(|(mint, _)| !followed.get(mint).is_some_and(|state| state.pool.is_some()));
        }
        out.sort();
        out
    }

    /// Hunt the migration for every bag without a pool leg and register
    /// whatever resolves (bounded per bag by the existing hunt
    /// deadline). Returns how many bags still lack an exit feed after
    /// the pass; the caller alerts when non-zero and retries on the
    /// supervision recheck.
    pub async fn refresh_recovered_bag_feeds(&self) -> usize {
        let bags = self.bags_without_pool_leg().await;
        for (mint, curve) in bags {
            self.report_migration_once(mint, curve).await;
        }
        self.bags_without_pool_leg().await.len()
    }

    /// Sweep idle follows even when no pool ticks arrive (call from the
    /// poll loop on a timer). `on_pool_tick` also sweeps, but a fully
    /// idle ticker would otherwise leak `followed` + vault subs to restart.
    pub async fn sweep_idle(&self) -> Vec<String> {
        self.evict_idle_follows(now_secs()).await
    }

    /// Idle follows past TTL without live custody, evicted (unwatch +
    /// map remove). Candidates collected under a short guard; custody
    /// checks await after it drops (never hold a lock across await).
    /// Skips mints with live custody: evicting a held bag's leg strands
    /// its exit to manual.
    async fn evict_idle_follows(&self, now: u64) -> Vec<String> {
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
                Some(trader) => self.has_live_exposure(trader, &mint).await,
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
        // Evict idle follows first. Skips mints with live custody:
        // evicting a held bag's leg strands its Raydium exit to manual.
        // Same custody rule as the spike path: any tracked, parked,
        // or pending bag keeps its leg.
        let _ = self.evict_idle_follows(now).await;
        let mut followed = self.followed.lock().await;
        let state = followed.entry(tick.mint.clone()).or_insert(FollowState {
            stage: FollowStage::Seeding,
            last_seen_secs: now,
            // Ticks never create pool legs.
            pool: None,
        });
        state.last_seen_secs = now;
        let entry_price = match state.stage {
            // First print seeds baseline, never alerts.
            FollowStage::Seeding => {
                state.stage = FollowStage::Watching {
                    entry_price: tick.price_usd,
                };
                return;
            }
            FollowStage::Alerted => return,
            FollowStage::Watching { entry_price } => entry_price,
        };
        if tick.price_usd >= entry_price * FOLLOW_SPIKE_MULT {
            state.stage = FollowStage::Alerted;
            let mint = tick.mint.clone();
            drop(followed);
            info!(mint = %mint, multiple = FOLLOW_SPIKE_MULT, price = tick.price_usd, "post-migration spike");
            self.emit(Alerter::format_follow_spike(
                &self.card_ctx(),
                &mint,
                FOLLOW_SPIKE_MULT,
                tick.price_usd,
            ));
            // Release unless live still needs the leg.
            let keep_for_exit = match &self.exec.live {
                Some(trader) => self.has_live_exposure(trader, &mint).await,
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
            .is_some_and(|state| matches!(state.stage, FollowStage::Alerted));
        if !alerted {
            return;
        }
        let reap = match &self.exec.live {
            Some(trader) => !self.has_live_exposure(trader, mint).await,
            None => true,
        };
        if reap {
            self.vaults.unwatch(mint).await;
            self.followed.lock().await.remove(mint);
            info!(mint = %mint, "post-spike follow released with settled custody");
        }
    }
}

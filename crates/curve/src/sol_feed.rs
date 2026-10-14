//! SOL/USD price feed.
//! `None` when stale; no fallback price.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// WSOL mint (price key).
pub const SOL_MINT: &str = "So11111111111111111111111111111111111111112";
/// Jupiter price endpoint.
pub const DEFAULT_JUP_URL: &str =
    "https://lite-api.jup.ag/price/v3?ids=So11111111111111111111111111111111111111112";
/// Stale age cutoff.
pub const DEFAULT_STALE_SECS: u64 = 120;
/// Poll interval.
pub const DEFAULT_REFRESH: Duration = Duration::from_secs(30);

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

struct Inner {
    price_bits: AtomicU64,
    updated_at: AtomicU64,
}

/// Cloneable SOL/USD feed.
#[derive(Clone)]
pub struct SolUsdFeed {
    inner: Arc<Inner>,
    client: reqwest::Client,
    url: String,
    stale_secs: u64,
    refresh: Duration,
}

impl std::fmt::Debug for SolUsdFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SolUsdFeed")
            .field("price", &self.price())
            .field("url", &self.url)
            .field("stale_secs", &self.stale_secs)
            .finish()
    }
}

impl SolUsdFeed {
    /// New feed with no price yet.
    pub fn new(url: &str, stale_secs: u64, refresh: Duration) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(Inner {
                price_bits: AtomicU64::new(0),
                updated_at: AtomicU64::new(0),
            }),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            url: url.to_string(),
            stale_secs,
            refresh,
        })
    }

    /// Production defaults.
    pub fn with_defaults() -> Arc<Self> {
        Self::new(DEFAULT_JUP_URL, DEFAULT_STALE_SECS, DEFAULT_REFRESH)
    }

    /// Test feed seeded with `price`. No network.
    pub fn new_for_test(price: f64) -> Arc<Self> {
        let feed = Self::new(DEFAULT_JUP_URL, DEFAULT_STALE_SECS, DEFAULT_REFRESH);
        if price.is_finite() && price > 0.0 {
            feed.inner.price_bits.store(price.to_bits(), Ordering::Relaxed);
            feed.inner.updated_at.store(now_secs(), Ordering::Relaxed);
        }
        feed
    }

    /// Current price if fresh.
    pub fn price(&self) -> Option<f64> {
        self.stamped_price().map(|(price, _)| price)
    }

    /// Price with its timestamp.
    pub fn stamped_price(&self) -> Option<(f64, u64)> {
        let bits = self.inner.price_bits.load(Ordering::Relaxed);
        if bits == 0 {
            return None;
        }
        let price = f64::from_bits(bits);
        if !price.is_finite() || price <= 0.0 {
            return None;
        }
        let updated = self.inner.updated_at.load(Ordering::Relaxed);
        if updated == 0 {
            return None;
        }
        let age = now_secs().saturating_sub(updated);
        if age >= self.stale_secs {
            return None;
        }
        Some((price, updated))
    }

    /// True if a `stamped_price` timestamp is still fresh.
    /// Check at use time.
    pub fn age_ok(&self, updated_at: u64) -> bool {
        now_secs().saturating_sub(updated_at) < self.stale_secs
    }

    /// Seconds since last fetch (MAX if never).
    pub fn age_secs(&self) -> u64 {
        let updated = self.inner.updated_at.load(Ordering::Relaxed);
        if updated == 0 {
            return u64::MAX;
        }
        now_secs().saturating_sub(updated)
    }

    /// True when no fresh price is available.
    pub fn is_stale(&self) -> bool {
        self.price().is_none()
    }

    /// Force-set price. Updates timestamp to now.
    pub fn set_price(&self, price: f64) {
        if price.is_finite() && price > 0.0 {
            self.inner.price_bits.store(price.to_bits(), Ordering::Relaxed);
            self.inner.updated_at.store(now_secs(), Ordering::Relaxed);
        }
    }

    /// One HTTP fetch. Parses `usdPrice` for SOL.
    pub async fn fetch_once(&self) -> Result<f64, String> {
        let resp = self
            .client
            .get(&self.url)
            .send()
            .await
            .map_err(|e| format!("jup fetch failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("jup status {}", resp.status()));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("jup json failed: {e}"))?;
        // Shape: {mint: {usdPrice, ...}}
        let entry = body
            .get(SOL_MINT)
            .ok_or_else(|| format!("jup missing key {SOL_MINT}"))?;
        let usd = entry
            .get("usdPrice")
            .and_then(|v| v.as_f64())
            .ok_or_else(|| "jup missing usdPrice".to_string())?;
        if !usd.is_finite() || usd <= 0.0 {
            return Err(format!("jup bad price {usd}"));
        }
        Ok(usd)
    }

    /// Fetch and store; returns the fresh price on success.
    pub async fn refresh_once(&self) -> Option<f64> {
        match self.fetch_once().await {
            Ok(price) => {
                self.set_price(price);
                tracing::debug!(price, age = 0, "sol_usd refreshed");
                Some(price)
            }
            Err(e) => {
                tracing::warn!(error = %e, age = self.age_secs(), "sol_usd refresh failed");
                None
            }
        }
    }

    /// Background loop. Ends when the feed drops.
    pub async fn run(self: Arc<Self>) {
        let _ = self.refresh_once().await;
        let refresh = self.refresh;
        let weak = Arc::downgrade(&self);
        drop(self);
        let mut interval = tokio::time::interval(refresh);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            let Some(feed) = weak.upgrade() else {
                break;
            };
            interval.tick().await;
            let _ = feed.refresh_once().await;
        }
    }

    /// Spawn the background refresher.
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        let feed = Arc::clone(&self);
        tokio::spawn(async move { feed.run().await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_price_available() {
        let feed = SolUsdFeed::new_for_test(150.0);
        assert_eq!(feed.price(), Some(150.0));
        assert!(!feed.is_stale());
    }

    #[test]
    fn unset_and_stale_read_none() {
        // Fresh-but-empty and aged feeds both price None + stale.
        let feed = SolUsdFeed::new(DEFAULT_JUP_URL, 120, Duration::from_secs(30));
        assert_eq!(feed.price(), None);
        assert!(feed.is_stale());
        let feed = SolUsdFeed::new_for_test(100.0);
        feed.inner
            .updated_at
            .store(now_secs().saturating_sub(9999), Ordering::Relaxed);
        assert_eq!(feed.price(), None);
        assert!(feed.is_stale());
    }

    #[test]
    fn set_price_updates() {
        let feed = SolUsdFeed::new(DEFAULT_JUP_URL, 120, Duration::from_secs(30));
        feed.set_price(200.0);
        assert_eq!(feed.price(), Some(200.0));
    }

    #[test]
    fn sol_mint_matches_ids_wsol() {
        // String twin of `ids::WSOL_MINT` (JSON key + URL need strings):
        // drift breaks loudly here, not in prod.
        assert_eq!(SOL_MINT, ids::WSOL_MINT.to_string());
        assert!(DEFAULT_JUP_URL.ends_with(&ids::WSOL_MINT.to_string()));
    }
}

//! SOL/USD price feed.
//! `None` when stale; no fallback price.

use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// WSOL mint (price key). Single source is `ids::WSOL_MINT`;
/// this string twin exists because the price-API JSON key + URL need
/// strings. Drift is pinned by `sol_mint_matches_ids_wsol` below.
pub static SOL_MINT: LazyLock<String> = LazyLock::new(|| ids::WSOL_MINT.to_string());
/// DexScreener spot endpoint (free, keyless, no new deps). Built from
/// `ids::WSOL_MINT` so a mint rotation updates the key and the URL
/// together. Replaces the retired Jupiter Lite endpoint (gated with
/// 403/525 in Oct-2026): same fail-closed stale contract, different
/// vendor. Polls at 2 req/min, far inside public limits.
pub static DEFAULT_DEX_URL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "https://api.dexscreener.com/latest/dex/tokens/{}",
        ids::WSOL_MINT
    )
});
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

/// DexScreener tokens body: only `pairs` matters for the price walk.
#[derive(Debug, serde::Deserialize)]
struct DexScreenerTokens {
    /// Missing/null fails closed (no legs to walk).
    #[serde(default)]
    pairs: Option<Vec<DexScreenerPair>>,
}

/// One tokens leg. A non-object element (schema drift) is skipped by
/// the walk, never fatal for the whole body.
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum DexScreenerPair {
    Pair {
        #[serde(rename = "quoteToken", default)]
        quote_token: Option<QuoteToken>,
        #[serde(rename = "priceUsd", default)]
        price_usd: Option<PriceUsd>,
    },
    Other(serde::de::IgnoredAny),
}

#[derive(Debug, serde::Deserialize)]
struct QuoteToken {
    /// Quoted asset: `USDC` is preferred, anything else is the fallback.
    #[serde(default)]
    symbol: Option<String>,
}

/// `priceUsd` ships as a string; a bare number rides along without
/// failing the feed closed (schema drift), anything else is ignored.
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum PriceUsd {
    Text(String),
    Number(f64),
    Other(serde::de::IgnoredAny),
}

impl PriceUsd {
    fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Text(s) => s.parse().ok(),
            Self::Number(n) => Some(*n),
            Self::Other(_) => None,
        }
    }
}

/// First USDC-quoted SOL price from a DexScreener tokens response,
/// else USDT-quoted. Pure (unit-tested against the live shape):
/// `None` on missing, unpriced, unparseable, or unknown-quote legs —
/// the caller fails closed. Only deep stablecoin quotes (USDC, USDT)
/// are trusted: a thin/unknown-quote leg must never set the global
/// price used for USD sizing, guards, and paper valuation.
fn parse_dexscreener_sol_usd(body: &str) -> Option<f64> {
    let tokens: DexScreenerTokens = serde_json::from_str(body).ok()?;
    let mut usdt = None;
    for pair in tokens.pairs?.iter() {
        let DexScreenerPair::Pair {
            quote_token,
            price_usd,
        } = pair
        else {
            continue;
        };
        // A leg without a quote symbol was never a priced pair here:
        // skip it, never guess its quote side.
        let Some(symbol) = quote_token.as_ref().and_then(|q| q.symbol.as_deref()) else {
            continue;
        };
        let Some(price) = price_usd.as_ref().and_then(PriceUsd::as_f64) else {
            continue;
        };
        if !price.is_finite() || price <= 0.0 {
            continue;
        }
        if symbol == "USDC" {
            return Some(price);
        }
        if symbol == "USDT" {
            usdt = usdt.or(Some(price));
        }
        // Any other quote (SOL, XYZ, ...): untrusted for the global
        // price — skipped, never a fallback.
    }
    usdt
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
        Self::new(&DEFAULT_DEX_URL, DEFAULT_STALE_SECS, DEFAULT_REFRESH)
    }

    /// Test feed seeded with `price`. No network.
    pub fn new_for_test(price: f64) -> Arc<Self> {
        let feed = Self::new(&DEFAULT_DEX_URL, DEFAULT_STALE_SECS, DEFAULT_REFRESH);
        if price.is_finite() && price > 0.0 {
            feed.inner
                .price_bits
                .store(price.to_bits(), Ordering::Relaxed);
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

    /// True once any price was ever recorded. Distinguishes "feed lost"
    /// (degrade: stop deciding) from "never had one since boot"
    /// (grace: the supervisor allows a boot window before pausing).
    pub fn has_ever_priced(&self) -> bool {
        self.inner.updated_at.load(Ordering::Relaxed) != 0
    }

    /// Force-set price. Updates timestamp to now.
    pub fn set_price(&self, price: f64) {
        if price.is_finite() && price > 0.0 {
            self.inner
                .price_bits
                .store(price.to_bits(), Ordering::Relaxed);
            self.inner.updated_at.store(now_secs(), Ordering::Relaxed);
        }
    }

    /// One HTTP fetch. Parses the USDC-quoted (else USDT-quoted) SOL
    /// price. Shape: `{pairs: [{quoteToken: {symbol}, priceUsd: "..."}]}`.
    /// Stablecoin quotes only (deepest SOL spot); unknown quotes fail
    /// closed instead of setting the global price.
    pub async fn fetch_once(&self) -> Result<f64, String> {
        let resp = self
            .client
            .get(&self.url)
            .send()
            .await
            .map_err(|e| format!("dex fetch failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("dex status {}", resp.status()));
        }
        let body = resp
            .text()
            .await
            .map_err(|e| format!("dex body failed: {e}"))?;
        let usd = parse_dexscreener_sol_usd(&body)
            .ok_or_else(|| "dex missing stablecoin-quoted SOL price".to_string())?;
        if !usd.is_finite() || usd <= 0.0 {
            return Err(format!("dex bad price {usd}"));
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
        assert!(feed.has_ever_priced());
    }

    #[test]
    fn ever_priced_distinguishes_boot_from_loss() {
        // Never priced since boot: startup, not degradation (the
        // supervisor grants it a grace window).
        let feed = SolUsdFeed::new(&DEFAULT_DEX_URL, 120, Duration::from_secs(30));
        assert!(!feed.has_ever_priced());
        assert!(feed.is_stale());
        feed.set_price(200.0);
        assert!(feed.has_ever_priced());
        // Aged out afterwards: lost a HAD price — degrade immediately.
        feed.inner
            .updated_at
            .store(now_secs().saturating_sub(9999), Ordering::Relaxed);
        assert!(feed.is_stale());
        assert!(feed.has_ever_priced(), "history survives staleness");
    }

    #[test]
    fn unset_and_stale_read_none() {
        // Fresh-but-empty and aged feeds both price None + stale.
        let feed = SolUsdFeed::new(&DEFAULT_DEX_URL, 120, Duration::from_secs(30));
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
        let feed = SolUsdFeed::new(&DEFAULT_DEX_URL, 120, Duration::from_secs(30));
        feed.set_price(200.0);
        assert_eq!(feed.price(), Some(200.0));
    }

    #[test]
    fn sol_mint_matches_ids_wsol() {
        // String twin of `ids::WSOL_MINT` (JSON key + URL need strings):
        // drift breaks loudly here, not in prod.
        assert_eq!(SOL_MINT.as_str(), ids::WSOL_MINT.to_string());
        assert!(DEFAULT_DEX_URL.ends_with(ids::WSOL_MINT.to_string().as_str()));
    }

    /// Live DexScreener shape, trimmed to two legs (fetched 2026-10-16;
    /// SOL ~$106 across USDC pairs). Proves the parser against the wire,
    /// not a hand-invented schema.
    const DEX_SAMPLE: &str = r#"{"pairs":[
        {"chainId":"solana","dexId":"orca","quoteToken":{"symbol":"USDC"},"priceUsd":"106.21"},
        {"chainId":"solana","dexId":"raydium","quoteToken":{"symbol":"SOL"},"priceUsd":"1.0"}
    ]}"#;

    #[test]
    fn dex_parser_prefers_usdc_leg() {
        assert_eq!(parse_dexscreener_sol_usd(DEX_SAMPLE), Some(106.21));
    }

    #[test]
    fn dex_parser_falls_back_to_usdt_only() {
        // USDT is a trusted stablecoin fallback for USDC.
        assert_eq!(
            parse_dexscreener_sol_usd(
                r#"{"pairs":[{"quoteToken":{"symbol":"USDT"},"priceUsd":"99.5"}]}"#
            ),
            Some(99.5)
        );
        // Unknown quotes never set the global price — fail closed even
        // when they are the only priced leg (one thin pair must not
        // move every USD size, guard, and paper valuation).
        for raw in [
            r#"{"pairs":[{"quoteToken":{"symbol":"XYZ"},"priceUsd":"99.5"}]}"#,
            r#"{"pairs":[{"quoteToken":{"symbol":"SOL"},"priceUsd":"106.2"}]}"#,
            r#"{"pairs":[
                {"quoteToken":{"symbol":"XYZ"},"priceUsd":"99.5"},
                {"quoteToken":{"symbol":"SOL"},"priceUsd":"106.2"}
            ]}"#,
        ] {
            assert_eq!(parse_dexscreener_sol_usd(raw), None, "{raw}");
        }
        // Bare numbers parse too (schema drift must not fail closed).
        assert_eq!(
            parse_dexscreener_sol_usd(
                r#"{"pairs":[{"quoteToken":{"symbol":"USDC"},"priceUsd":106.5}]}"#
            ),
            Some(106.5)
        );
    }

    #[test]
    fn dex_parser_skips_drifted_legs_not_the_body() {
        // Non-object legs and non-scalar `priceUsd` values are skipped
        // per leg; the next priced leg still wins, so shape drift never
        // fails the whole response closed.
        assert_eq!(
            parse_dexscreener_sol_usd(
                r#"{"pairs":[42,{"priceUsd":true},{"quoteToken":{"symbol":"USDC"},"priceUsd":"106"}]}"#
            ),
            Some(106.0)
        );
    }

    #[test]
    fn dex_parser_rejects_garbage() {
        for raw in [
            r#"{"pairs":[]}"#,
            r#"{}"#,
            r#"{"pairs":[{"quoteToken":{"symbol":"USDC"}}]}"#,
            r#"{"pairs":[{"priceUsd":"99"}]}"#,
            r#"{"pairs":[{"quoteToken":{"symbol":"USDC"},"priceUsd":"abc"}]}"#,
            r#"{"pairs":[{"quoteToken":{"symbol":"USDC"},"priceUsd":"-3"}]}"#,
            r#"{"pairs":[{"quoteToken":{"symbol":"USDC"},"priceUsd":"0"}]}"#,
        ] {
            assert_eq!(parse_dexscreener_sol_usd(raw), None, "{raw}");
        }
    }
}

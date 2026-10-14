//! Reserve polling ticker over HTTP RPC.

use crate::backoff::{RoundBackoff, warn_throttled};
use crate::layout::{BondingCurveReserves, CurveParseError};
use crate::sol_feed::SolUsdFeed;
use solana_client::client_error::ClientError;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock, mpsc};
use tracing::{debug, warn};

// Warn slots: persistent fee/read trouble pages ops, not every round.
static FEE_WARN_AT: AtomicU64 = AtomicU64::new(0);
static TICK_WARN_AT: AtomicU64 = AtomicU64::new(0);

/// Ticker failures.
#[derive(Error, Debug)]
pub enum CurveError {
    #[error("invalid rpc url: {0}")]
    BadUrl(String),
    #[error("invalid curve key '{0}'")]
    BadCurveKey(String),
    #[error("invalid mint key '{0}'")]
    BadMintKey(String),
    #[error("SOL/USD feed stale: no fresh price")]
    StaleSolUsd,
    #[error("rpc error: {0}")]
    Rpc(#[from] ClientError),
    #[error("rpc read timed out")]
    Timeout,
    #[error("curve account missing: {0}")]
    AccountMissing(String),
    #[error("reserve parse failed: {0}")]
    Parse(#[from] CurveParseError),
    #[error("no spot price for curve: {0}")]
    NoPrice(String),
}

/// One reserve observation, priced in USD.
#[derive(Debug, Clone)]
pub struct PriceTick {
    pub mint: String,
    pub curve: String,
    /// Spot USD per whole token.
    pub price_usd: f64,
    /// SOL-side liquidity in USD.
    pub liquidity_usd: f64,
    /// True once migrated.
    pub complete: bool,
    /// Local unix seconds.
    pub observed_at: u64,
}

/// Watched curve entry.
#[derive(Debug, Clone)]
struct Watched {
    mint: String,
    curve: Pubkey,
}

/// Polling price ticker.
#[derive(Clone)]
pub struct CurveTicker {
    rpc: Arc<RpcClient>,
    subs: Arc<RwLock<HashMap<String, Watched>>>,
    interval: Duration,
    sol_feed: Arc<SolUsdFeed>,
    dropped: Arc<std::sync::atomic::AtomicU64>,
    /// Mint decimals cache (immutable; evicted past cap).
    decimals: Arc<Mutex<HashMap<String, u8>>>,
    /// FeeConfig/Global cache.
    fee_inputs: crate::fee::FeeInputsCache,
    /// Mint supply cache (immutable on curves; evicted past cap).
    supply: crate::fee::SupplyCache,
    /// Consecutive-failure backoff (sick node, not sick data).
    backoff: Arc<RoundBackoff>,
}

/// Decimals cache cap.
pub const DECIMALS_CACHE_CAP: usize = 5_000;

impl CurveTicker {
    /// Map commitment name to config.
    pub fn parse_commitment(s: &str) -> Option<CommitmentConfig> {
        match s {
            "processed" => Some(CommitmentConfig::processed()),
            "finalized" => Some(CommitmentConfig::finalized()),
            "confirmed" => Some(CommitmentConfig::confirmed()),
            _ => None,
        }
    }

    /// Build a ticker.
    pub fn new(
        http_url: &str,
        commitment: CommitmentConfig,
        interval: Duration,
        sol_feed: Arc<SolUsdFeed>,
    ) -> Result<Self, CurveError> {
        match url::Url::parse(http_url) {
            Ok(u) if u.scheme() == "http" || u.scheme() == "https" => {}
            _ => return Err(CurveError::BadUrl(http_url.to_string())),
        }
        Ok(Self {
            rpc: Arc::new(RpcClient::new_with_commitment(
                http_url.to_string(),
                commitment,
            )),
            subs: Arc::new(RwLock::new(HashMap::new())),
            interval,
            sol_feed,
            dropped: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            decimals: Arc::new(Mutex::new(HashMap::new())),
            fee_inputs: Arc::new(Mutex::new(None)),
            supply: Arc::new(Mutex::new(HashMap::new())),
            backoff: Arc::new(RoundBackoff::new()),
        })
    }

    /// Ticks dropped on a full queue. Test-only: production surfaces
    /// drops via tracing, never by polling this counter.
    #[cfg(test)]
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Watch a curve for a mint.
    pub async fn watch(&self, mint: &str, curve: &str) -> Result<(), CurveError> {
        Pubkey::from_str(mint).map_err(|_| CurveError::BadMintKey(mint.to_string()))?;
        let key =
            Pubkey::from_str(curve).map_err(|_| CurveError::BadCurveKey(curve.to_string()))?;
        let mut subs = self.subs.write().await;
        if let Some(existing) = subs.get(mint)
            && existing.curve != key
        {
            debug!(mint = %mint, old = %existing.curve, new = %key, "curve re-watch replaces address");
        }
        subs.insert(
            mint.to_string(),
            Watched {
                mint: mint.to_string(),
                curve: key,
            },
        );
        Ok(())
    }

    /// Stop watching a mint.
    pub async fn unwatch(&self, mint: &str) -> bool {
        self.subs.write().await.remove(mint).is_some()
    }

    /// Watched mint count.
    pub async fn watched_count(&self) -> usize {
        self.subs.read().await.len()
    }


    /// One-shot reserve read (one retry, with deadline).
    pub async fn fetch_tick(&self, mint: &str, curve: &str) -> Result<PriceTick, CurveError> {
        Pubkey::from_str(mint).map_err(|_| CurveError::BadMintKey(mint.to_string()))?;
        let key =
            Pubkey::from_str(curve).map_err(|_| CurveError::BadCurveKey(curve.to_string()))?;
        let sol_usd = self.sol_feed.price().ok_or(CurveError::StaleSolUsd)?;
        let account = read_with_retry(&self.rpc, &key).await?;
        let decimals = self.token_decimals(mint).await?;
        tick_from_account(mint, curve, &account.data, &account.owner, sol_usd, decimals)
    }

    /// Mint decimals (cached). Fail-closed.
    pub async fn token_decimals(&self, mint: &str) -> Result<u8, CurveError> {
        if let Some(hit) = self.decimals.lock().await.get(mint) {
            return Ok(*hit);
        }
        let key = Pubkey::from_str(mint).map_err(|_| CurveError::BadMintKey(mint.to_string()))?;
        let account = match tokio::time::timeout(DECIMALS_TIMEOUT, self.rpc.get_account(&key)).await
        {
            Ok(Ok(account)) => account,
            Ok(Err(e)) => return Err(CurveError::Rpc(e)),
            Err(_) => return Err(CurveError::Timeout),
        };
        let decimals = account
            .data
            .get(MINT_DECIMALS_OFFSET)
            .copied()
            .ok_or(CurveError::Parse(CurveParseError::DataTooShort(
                account.data.len(),
            )))?;
        let mut cache = self.decimals.lock().await;
        if cache.len() >= DECIMALS_CACHE_CAP
            && let Some(oldest) = cache.keys().next().cloned()
        {
            cache.remove(&oldest);
        }
        cache.insert(mint.to_string(), decimals);
        Ok(decimals)
    }

    /// Venue fee for one curve. `None` keeps 125 bps.
    pub async fn venue_fee_bps(&self, mint: &str, curve: &str) -> Option<f64> {
        let mint_key = Pubkey::from_str(mint)
            .map_err(|_| CurveError::BadMintKey(mint.to_string()))
            .ok()?;
        let key = Pubkey::from_str(curve)
            .map_err(|_| CurveError::BadCurveKey(curve.to_string()))
            .ok()?;
        let fee =
            crate::fee::venue_fee_bps(&self.rpc, &self.fee_inputs, &self.supply, &mint_key, &key)
                .await;
        if fee.is_none() && warn_throttled(&FEE_WARN_AT) {
            warn!(%curve, "venue fee unreadable; paper keeps the standard tier");
        }
        fee
    }

    /// Reserve read plus venue fee from the same bytes.
    pub async fn fetch_tick_with_fee(
        &self,
        mint: &str,
        curve: &str,
    ) -> Result<(PriceTick, Option<f64>), CurveError> {
        Pubkey::from_str(mint).map_err(|_| CurveError::BadMintKey(mint.to_string()))?;
        let key =
            Pubkey::from_str(curve).map_err(|_| CurveError::BadCurveKey(curve.to_string()))?;
        let sol_usd = self.sol_feed.price().ok_or(CurveError::StaleSolUsd)?;
        let account = read_with_retry(&self.rpc, &key).await?;
        let decimals = self.token_decimals(mint).await?;
        let tick =
            tick_from_account(mint, curve, &account.data, &account.owner, sol_usd, decimals)?;
        let mint_key = Pubkey::from_str(mint).expect("mint validated above");
        let fee = crate::fee::venue_fee_for_data(
            &self.rpc,
            &self.fee_inputs,
            &self.supply,
            &mint_key,
            &account.owner,
            &account.data,
        )
        .await;
        if fee.is_none() && warn_throttled(&FEE_WARN_AT) {
            warn!(%curve, "venue fee unreadable; paper keeps the standard tier");
        }
        Ok((tick, fee))
    }

    /// Poll loop. Ends when the receiver drops.
    pub async fn run(self, tx: mpsc::Sender<PriceTick>) {
        let mut timer = tokio::time::interval(self.interval);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        timer.tick().await;
        loop {
            timer.tick().await;
            if tx.is_closed() {
                debug!("tick receiver closed; ticker stopping");
                return;
            }
            let watched: Vec<Watched> = self.subs.read().await.values().cloned().collect();
            if watched.is_empty() {
                continue;
            }
            let Some(sol_usd) = self.sol_feed.price() else {
                if warn_throttled(&TICK_WARN_AT) {
                    warn!("sol_usd stale; skipping pool rounds until the feed recovers");
                } else {
                    debug!("sol_usd stale; skipping round");
                }
                continue;
            };
            let keys: Vec<Pubkey> = watched.iter().map(|w| w.curve).collect();
            let accounts =
                match tokio::time::timeout(ROUND_OVERALL_TIMEOUT, poll_round(&self.rpc, &keys))
                    .await
                {
                    Ok(Ok(accounts)) => {
                        self.backoff.success();
                        accounts
                    }
                    Ok(Err(e)) => {
                        if warn_throttled(&TICK_WARN_AT) {
                            warn!(error = %e, n = keys.len(), "round failed; backing off");
                        }
                        tokio::time::sleep(self.backoff.fail_sleep()).await;
                        continue;
                    }
                    Err(_) => {
                        if warn_throttled(&TICK_WARN_AT) {
                            warn!(n = keys.len(), "round timed out; backing off");
                        }
                        tokio::time::sleep(self.backoff.fail_sleep()).await;
                        continue;
                    }
                };
            if accounts.len() != watched.len() {
                warn!(
                    expected = watched.len(),
                    got = accounts.len(),
                    "account batch length mismatch; skipping round"
                );
                continue;
            }
            for (entry, maybe_account) in watched.iter().zip(accounts.iter()) {
                let Some(account) = maybe_account else {
                    if warn_throttled(&TICK_WARN_AT) {
                        warn!(mint = %entry.mint, "curve account missing for rounds; still unindexed?");
                    }
                    continue;
                };
                // Wrong-scale prints are worse than skips.
                let decimals = match self.token_decimals(&entry.mint).await {
                    Ok(d) => d,
                    Err(e) => {
                        if warn_throttled(&TICK_WARN_AT) {
                            warn!(mint = %entry.mint, error = %e, "mint decimals unreadable for rounds; skipping ticks");
                        }
                        continue;
                    }
                };
                match tick_from_account(
                    &entry.mint,
                    &entry.curve.to_string(),
                    &account.data,
                    &account.owner,
                    sol_usd,
                    decimals,
                ) {
                    Ok(tick) => match tx.try_send(tick) {
                        Ok(()) => {}
                        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                            self.dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            debug!("tick queue full; dropping tick");
                        }
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                            debug!("tick receiver closed; ticker stopping");
                            return;
                        }
                    },
                    Err(e) => {
                        if warn_throttled(&TICK_WARN_AT) {
                            warn!(mint = %entry.mint, error = %e, "tick parse failing for rounds; layout changed?");
                        }
                    }
                }
            }
        }
    }
}

/// Overall round deadline.
pub const ROUND_OVERALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Per-round RPC deadline.
pub const ROUND_TIMEOUT: Duration = Duration::from_secs(15);

/// Offset of `decimals` in a mint account (shared by both SPL programs).
pub const MINT_DECIMALS_OFFSET: usize = 44;

/// Deadline for one decimals read.
pub const DECIMALS_TIMEOUT: Duration = Duration::from_secs(5);

/// Single account read with one retry on transient failures.
async fn read_with_retry(
    rpc: &RpcClient,
    key: &Pubkey,
) -> Result<solana_account::Account, CurveError> {
    let attempt = || async {
        match tokio::time::timeout(ROUND_TIMEOUT, rpc.get_account(key)).await {
            Ok(Ok(account)) => Ok(account),
            Ok(Err(e)) => Err(CurveError::Rpc(e)),
            Err(_) => Err(CurveError::Timeout),
        }
    };
    match attempt().await {
        Ok(account) => Ok(account),
        Err(e) if is_transient_read_error(&e) => {
            tokio::time::sleep(Duration::from_millis(250)).await;
            attempt().await
        }
        Err(e) => Err(e),
    }
}

/// True for read failures worth one retry.
fn is_transient_read_error(e: &CurveError) -> bool {
    match e {
        CurveError::Timeout => true,
        CurveError::Rpc(e) => match e.kind() {
            solana_client::client_error::ClientErrorKind::Io(_) => true,
            solana_client::client_error::ClientErrorKind::Reqwest(e) => {
                e.is_timeout() || e.is_connect()
            }
            solana_client::client_error::ClientErrorKind::RpcError(
                solana_client::rpc_request::RpcError::RpcResponseError {
                    code: -32005 | -32007 | -32009 | 429,
                    ..
                },
            ) => true,
            _ => false,
        },
        _ => false,
    }
}

/// Max accounts per multi-account call.
pub const BATCH_CHUNK_SIZE: usize = 100;

/// Batched multi-account read with a deadline.
async fn poll_round(
    rpc: &RpcClient,
    keys: &[Pubkey],
) -> Result<Vec<Option<solana_account::Account>>, CurveError> {
    let round = futures_util::future::join_all(keys.chunks(BATCH_CHUNK_SIZE.max(1)).map(
        |chunk| async move {
            match tokio::time::timeout(ROUND_TIMEOUT, rpc.get_multiple_accounts(chunk)).await {
                Ok(Ok(accounts)) => Ok(accounts),
                Ok(Err(e)) => Err(CurveError::Rpc(e)),
                Err(_) => Err(CurveError::Timeout),
            }
        },
    ))
    .await;
    let mut out = Vec::with_capacity(keys.len());
    for chunk in round {
        out.extend(chunk?);
    }
    Ok(out)
}

fn tick_from_account(
    mint: &str,
    curve: &str,
    data: &[u8],
    owner: &Pubkey,
    sol_usd: f64,
    decimals: u8,
) -> Result<PriceTick, CurveError> {
    let reserves = BondingCurveReserves::parse_validated(data, owner)?;
    let price_usd = reserves
        .price_usd(decimals, sol_usd)
        .ok_or_else(|| CurveError::NoPrice(curve.to_string()))?;
    Ok(PriceTick {
        mint: mint.to_string(),
        curve: curve.to_string(),
        price_usd,
        liquidity_usd: reserves.liquidity_usd(sol_usd),
        complete: reserves.complete,
        observed_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration as StdDuration;

    fn ticker() -> CurveTicker {
        CurveTicker::new(
            "https://api.mainnet-beta.solana.com",
            CommitmentConfig::confirmed(),
            StdDuration::from_millis(500),
            SolUsdFeed::new_for_test(150.0),
        )
        .expect("ticker")
    }

    #[test]
    fn rejects_bad_url() {
        assert!(matches!(
            CurveTicker::new(
                "notaurl",
                CommitmentConfig::confirmed(),
                StdDuration::from_secs(1),
                SolUsdFeed::new_for_test(150.0),
            ),
            Err(CurveError::BadUrl(_))
        ));
    }

    #[tokio::test]
    async fn watch_unwatch_roundtrip() {
        let t = ticker();
        let mint = Pubkey::new_from_array([2; 32]).to_string();
        assert_eq!(t.watched_count().await, 0);
        t.watch(&mint, "11111111111111111111111111111111")
            .await
            .expect("watch");
        assert_eq!(t.watched_count().await, 1);
        t.watch(&mint, "11111111111111111111111111111111")
            .await
            .expect("watch");
        assert_eq!(t.watched_count().await, 1);
        assert!(t.unwatch(&mint).await);
        assert!(!t.unwatch(&mint).await);
    }

    #[tokio::test]
    async fn rejects_bad_curve_key() {
        let t = ticker();
        let mint = Pubkey::new_from_array([2; 32]).to_string();
        assert!(matches!(
            t.watch(&mint, "not-a-pubkey").await,
            Err(CurveError::BadCurveKey(_))
        ));
    }

    #[test]
    fn tick_from_genesis_data() {
        let mut data = vec![23u8, 183, 248, 55, 96, 216, 172, 96];
        for v in [
            1_073_000_000_000_000u64,
            30_000_000_000,
            1_073_000_000_000_000,
            0,
            1_000_000_000_000_000,
        ] {
            data.extend_from_slice(&v.to_le_bytes());
        }
        data.push(0);
        let tick =
            tick_from_account("MINT", "CURVE", &data, &ids::PUMP_PROGRAM_ID, 150.0, 6).expect("tick");
        assert!((tick.price_usd - 2.7959e-8 * 150.0).abs() < 1e-12);
        assert!((tick.liquidity_usd - 4500.0).abs() < 1e-9);
        assert!(!tick.complete);
        let tick9 =
            tick_from_account("MINT", "CURVE", &data, &ids::PUMP_PROGRAM_ID, 150.0, 9).expect("tick");
        assert!((tick9.price_usd - tick.price_usd * 1000.0).abs() < 1e-9);
    }

    #[test]
    fn tick_rejects_garbage() {
        assert!(matches!(
            tick_from_account("M", "C", &[0u8; 8], &ids::PUMP_PROGRAM_ID, 150.0, 6),
            Err(CurveError::Parse(_))
        ));
    }

    #[test]
    fn tick_rejects_foreign_owner() {
        let mut data = vec![23u8, 183, 248, 55, 96, 216, 172, 96];
        data.extend_from_slice(&[0u8; 41]);
        let stranger = Pubkey::new_from_array([9; 32]);
        assert!(matches!(
            tick_from_account("M", "C", &data, &stranger, 150.0, 6),
            Err(CurveError::Parse(
                crate::layout::CurveParseError::WrongOwner(_)
            ))
        ));
    }

    #[test]
    fn feed_stale_fails_closed() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let stale = SolUsdFeed::new(
            crate::sol_feed::DEFAULT_JUP_URL,
            120,
            std::time::Duration::from_secs(30),
        );
        stale.set_price(150.0);
        let empty = SolUsdFeed::new(
            crate::sol_feed::DEFAULT_JUP_URL,
            120,
            std::time::Duration::from_secs(30),
        );
        let _ = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = stale;
        assert!(empty.price().is_none());
        assert!(empty.is_stale());
    }

    #[tokio::test]
    async fn rejects_bad_mint_key() {
        let t = ticker();
        assert!(matches!(
            t.watch("not-a-pubkey", "11111111111111111111111111111111").await,
            Err(CurveError::BadMintKey(_))
        ));
    }
}

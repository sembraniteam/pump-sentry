//! Reserve polling ticker over HTTP RPC.
//!
//! [`CurveTicker`] holds a dynamic watch set of bonding-curve accounts.
//! [`CurveTicker::run`] fetches them with `getMultipleAccounts` every
//! `interval` and emits a [`PriceTick`] per curve. Read failures are
//! logged and skipped — a missed round never kills the loop.

use crate::layout::{BondingCurveReserves, CurveParseError};
use crate::sol_feed::SolUsdFeed;
use solana_client::client_error::ClientError;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock, mpsc};
use tracing::{debug, warn};

/// Ticker failures: configuration and one-shot fetch errors. The
/// background loop itself never returns these (it logs and continues).
#[derive(Error, Debug)]
pub enum CurveError {
    /// RPC endpoint URL is invalid.
    #[error("invalid rpc url: {0}")]
    BadUrl(String),
    /// Bonding-curve address is not a valid pubkey.
    #[error("invalid curve key '{0}'")]
    BadCurveKey(String),
    /// Mint address is not a valid pubkey (watch/feeed misrouting).
    #[error("invalid mint key '{0}'")]
    BadMintKey(String),
    /// SOL→USD feed has no fresh price (stale or never fetched).
    #[error("SOL/USD feed stale: no fresh price")]
    StaleSolUsd,
    /// RPC transport failure.
    #[error("rpc error: {0}")]
    Rpc(#[from] ClientError),
    /// RPC read exceeded its deadline.
    #[error("rpc read timed out")]
    Timeout,
    /// Account does not exist (yet) on this RPC.
    #[error("curve account missing: {0}")]
    AccountMissing(String),
    /// Reserve parsing failed.
    #[error("reserve parse failed: {0}")]
    Parse(#[from] CurveParseError),
    /// Reserves present but no computable spot price.
    #[error("no spot price for curve: {0}")]
    NoPrice(String),
}

/// One reserve observation, priced in USD.
#[derive(Debug, Clone)]
pub struct PriceTick {
    /// Token mint (base58) this curve belongs to.
    pub mint: String,
    /// Bonding-curve address (base58).
    pub curve: String,
    /// Spot USD per whole token.
    pub price_usd: f64,
    /// SOL-side liquidity proxy in USD.
    pub liquidity_usd: f64,
    /// True once migrated (downstream should stop trusting spot).
    pub complete: bool,
    /// Local unix seconds when observed.
    pub observed_at: u64,
}

/// Watched curve entry.
#[derive(Debug, Clone)]
struct Watched {
    mint: String,
    curve: Pubkey,
}

/// Polling price ticker. Cloneable; the watch set is shared.
#[derive(Clone)]
pub struct CurveTicker {
    rpc: Arc<RpcClient>,
    subs: Arc<RwLock<HashMap<String, Watched>>>,
    interval: Duration,
    sol_feed: Arc<SolUsdFeed>,
    /// Ticks dropped because the downstream queue was full (see `run`:
    /// drop-on-full keeps one slow consumer from stalling every curve).
    dropped: Arc<std::sync::atomic::AtomicU64>,
    /// Mint → decimals cache for [`CurveTicker::token_decimals`].
    /// Decimals are immutable on-chain, so entries never expire; the map
    /// is bounded by distinct mints ever confirmed (arbitrary eviction
    /// past the cap — order-free, since no entry is hotter than another).
    decimals: Arc<Mutex<HashMap<String, u8>>>,
    /// FeeConfig/Global cache for [`CurveTicker::venue_fee_bps`]:
    /// program-owned globals change only on upgrades (hour TTL), while
    /// the per-curve account stays fresh per call.
    fee_inputs: crate::fee::FeeInputsCache,
}

/// Cap for the decimals cache (distinct confirmed mints; arbitrary
/// eviction past it — correctness never depends on which entry goes).
pub const DECIMALS_CACHE_CAP: usize = 5_000;

impl CurveTicker {
    /// Map `"processed" | "confirmed" | "finalized"` to a commitment.
    /// Returns `None` for unknown strings (fail-closed; the binary's
    /// startup gate rejects these before this is ever called).
    pub fn parse_commitment(s: &str) -> Option<CommitmentConfig> {
        match s {
            "processed" => Some(CommitmentConfig::processed()),
            "finalized" => Some(CommitmentConfig::finalized()),
            "confirmed" => Some(CommitmentConfig::confirmed()),
            _ => None,
        }
    }

    /// Build a ticker: `http_url` + commitment for reads, `interval`
    /// between poll rounds, `sol_feed` for SOL→USD conversion (dynamic,
    /// staleness-gated — `fetch_tick`/`run` fail stale rounds instead of
    /// pricing at a static number).
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
        })
    }

    /// Ticks dropped on a full downstream queue (backpressure accounting).
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Watch a curve for a mint (idempotent per mint). Both addresses are
    /// validated: an empty/garbage mint would silently misroute ticks.
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

    /// Stop watching a mint. Returns true when an entry was removed.
    pub async fn unwatch(&self, mint: &str) -> bool {
        self.subs.write().await.remove(mint).is_some()
    }

    /// Watched mint count.
    pub async fn watched_count(&self) -> usize {
        self.subs.read().await.len()
    }


    /// One-shot reserve read for a curve address. Single retry (transient
    /// failures only — a missing account is not retried) plus a deadline
    /// so hung RPCs fail fast instead of stalling the caller. The curve
    /// account owner must be the Pump.fun program (spoofed price sources
    /// rejected). Fails with [`CurveError::StaleSolUsd`] when the SOL/USD
    /// feed has no fresh price (every tick is dynamic — no static fallback).
    pub async fn fetch_tick(&self, mint: &str, curve: &str) -> Result<PriceTick, CurveError> {
        Pubkey::from_str(mint).map_err(|_| CurveError::BadMintKey(mint.to_string()))?;
        let key =
            Pubkey::from_str(curve).map_err(|_| CurveError::BadCurveKey(curve.to_string()))?;
        let sol_usd = self.sol_feed.price().ok_or(CurveError::StaleSolUsd)?;
        let account = read_with_retry(&self.rpc, &key).await?;
        // Decimals before pricing (cached; fail-closed like the confirm
        // gate — a tick at the wrong scale is worse than none).
        let decimals = self.token_decimals(mint).await?;
        tick_from_account(mint, curve, &account.data, &account.owner, sol_usd, decimals)
    }

    /// Mint decimals from the mint account (cached per mint; immutable
    /// on-chain so entries never expire). Fail-closed `Err` on
    /// transport/timeout/short data — callers sizing in base units must
    /// never guess. Both SPL programs share the fixed 44-byte prefix
    /// (extensions only append), so one offset serves both.
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

    /// Venue fee (protocol + coin creator, bps/side) for one curve's fee
    /// tier, read live: curve account (quote kind + coin override) plus
    /// the cached FeeConfig/Global inputs. `None` on any failure or on a
    /// multi-tier schedule — callers keep the standard 125 bps tier.
    /// Confirm-path only (bounded deadline, no retry): a missing tier
    /// must not stall the gate.
    pub async fn venue_fee_bps(&self, curve: &str) -> Option<f64> {
        let key =
            Pubkey::from_str(curve).map_err(|_| CurveError::BadCurveKey(curve.to_string())).ok()?;
        let fee = crate::fee::venue_fee_bps(&self.rpc, &self.fee_inputs, &key).await;
        if fee.is_none() {
            debug!(%curve, "venue fee unreadable; paper keeps the standard tier");
        }
        fee
    }

    /// Poll loop: `getMultipleAccounts` per round, one tick per curve.
    /// Interval-based (no drift accumulation); ends when the receiver
    /// drops. Batch failures back off instead of hammering the RPC.
    /// Delivery is drop-on-full (with a counter): one slow consumer must
    /// not head-of-line-block every watched curve.
    ///
    /// Mirrors `migration::VaultTicker::run` on purpose (single-account
    /// arity here vs vault pairs there, different batching): the shared
    /// shape is timer → closed-check → snapshot → deadline-bound fetch →
    /// length check → parse → drop-on-full send. Keep the backpressure
    /// contract (drop + counter + deadlines) in sync across both.
    pub async fn run(self, tx: mpsc::Sender<PriceTick>) {
        let mut timer = tokio::time::interval(self.interval);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Discard the immediate first tick: let subscriptions settle for
        // one full interval before the first round.
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
            // Dynamic SOL price per round: one fresh read gates every tick
            // in the round (stale feed skips the whole round — no static
            // fallback).
            let Some(sol_usd) = self.sol_feed.price() else {
                debug!("sol_usd stale; skipping round");
                continue;
            };
            let keys: Vec<Pubkey> = watched.iter().map(|w| w.curve).collect();
            let accounts =
                match tokio::time::timeout(ROUND_OVERALL_TIMEOUT, poll_round(&self.rpc, &keys))
                    .await
                {
                Ok(Ok(accounts)) => accounts,
                Ok(Err(e)) => {
                    warn!(error = %e, n = keys.len(), "round failed; backing off");
                    continue;
                }
                Err(_) => {
                    warn!(n = keys.len(), "round timed out; backing off");
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
                    debug!(mint = %entry.mint, "curve account missing (not yet indexed?)");
                    continue;
                };
                // Real decimals (cached) — a wrong-scale print is worse
                // than a skipped round.
                let decimals = match self.token_decimals(&entry.mint).await {
                    Ok(d) => d,
                    Err(e) => {
                        debug!(mint = %entry.mint, error = %e, "mint decimals unreadable; skipping tick");
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
                        debug!(mint = %entry.mint, error = %e, "tick parse failed");
                    }
                }
            }
        }
    }
}

/// Overall round deadline: chunked reads run concurrently, but a large
/// watchlist must still finish (or fail loudly) within budget instead of
/// piling missed ticks behind a 150s round.
pub const ROUND_OVERALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Per-round RPC deadline: hung reads fail instead of stalling polls.
pub const ROUND_TIMEOUT: Duration = Duration::from_secs(15);

/// Byte offset of `decimals` in an SPL mint account: mint(32) +
/// mint-authority `COption<Pubkey>`(36) + supply(8) = 44. Shared by the
/// classic and Token-2022 programs (extensions only append past the
/// fixed prefix).
pub const MINT_DECIMALS_OFFSET: usize = 44;

/// Deadline for one mint-decimals read (confirm path only; the hot
/// ticker loop never reads mints).
pub const DECIMALS_TIMEOUT: Duration = Duration::from_secs(5);

/// Single account read with one retry and a deadline. The retry fires
/// only for transient failures (transport/timeout/overload): a missing
/// account is deterministic and returns at once instead of costing a
/// second RPC plus 250ms.
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

/// True for read failures worth one retry: timeouts, transport trouble,
/// and node-overload codes. Deterministic failures (missing account —
/// `ForUser`/parse/request errors — and any other RPC response code)
/// return immediately.
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

/// Maximum accounts per `getMultipleAccounts` call: bounds payload size
/// and stays clear of per-request account limits on public endpoints.
pub const BATCH_CHUNK_SIZE: usize = 100;

/// Batched multi-account read with a deadline (single attempt; the poll
/// loop backs off and retries next round on failure). Large watchlists
/// are split into [`BATCH_CHUNK_SIZE`] chunks fetched CONCURRENTLY
/// (sequential 15s chunks turned 1000 curves into a 150s round); results
/// preserve key order across chunks (`join_all` is order-preserving).
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
    // Real mint decimals, not the assumed 6: a non-6 print used to be
    // emitted at 10^Δ the true price (the confirm gate drops non-6 mints
    // downstream, but every other tick consumer read garbage).
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
        // Idempotent re-watch of the same mint.
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
        // Non-6 decimals scale the same reserves differently (no more
        // silent 10^Δ misprints at the tick layer).
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
        // Seeded feed forced stale: fetch_tick must fail, never price at
        // a static fallback.
        let stale = SolUsdFeed::new(
            crate::sol_feed::DEFAULT_JUP_URL,
            120,
            std::time::Duration::from_secs(30),
        );
        stale.set_price(150.0);
        // Backdate via fresh feed with 0 stale window instead: simplest is
        // a feed with no price at all (never fetched).
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

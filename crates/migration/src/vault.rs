//! Post-migration pool price ticker over HTTP RPC.
//!
//! Reads both SPL vault balances of each watched Raydium pool in one
//! batched `getMultipleAccounts` round and converts them to USD spot.
//! Pump.fun tokens use 6 decimals, WSOL 9 (overridable per watch).
//! Missed rounds log and continue; the loop ends when the receiver drops.
//!
//! SPL token account layout (only bytes 64..72 are read):
//! `mint(32) owner(32) amount(u64 LE) ...` — 165 bytes total.

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use std::{collections::HashMap, str::FromStr, sync::Arc, time::Duration};
use thiserror::Error;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, warn};

/// SPL token account amount offset/length.
const TOKEN_AMOUNT_OFFSET: usize = 64;
/// Minimum account data holding the amount field.
const TOKEN_ACCOUNT_MIN_LEN: usize = 72;
/// Pump.fun token decimals.
pub const PUMP_TOKEN_DECIMALS_DEFAULT: u8 = 6;
/// WSOL decimals.
pub const WSOL_DECIMALS_DEFAULT: u8 = 9;

/// Vault ticker failures (configuration only; the loop never returns).
#[derive(Error, Debug)]
pub enum VaultError {
    /// RPC endpoint URL is invalid.
    #[error("invalid rpc url: {0}")]
    BadUrl(String),
    /// Address is not a valid pubkey: {0}
    #[error("invalid address: {0}")]
    BadAddress(String),
}

/// One pool price observation in USD.
#[derive(Debug, Clone)]
pub struct PoolTick {
    /// Token mint (base58).
    pub mint: String,
    /// Raydium AMM account (base58).
    pub amm_id: String,
    /// Spot USD per whole token.
    pub price_usd: f64,
    /// Quote-side liquidity proxy in USD.
    pub liquidity_usd: f64,
    /// Local unix seconds when observed.
    pub observed_at: u64,
}

/// Watched pool entry.
#[derive(Debug, Clone)]
struct WatchedPool {
    mint: String,
    amm_id: Pubkey,
    base_vault: Pubkey,
    quote_vault: Pubkey,
    base_decimals: u8,
    quote_decimals: u8,
}

/// Polling Raydium vault ticker. Cloneable; the watch set is shared.
#[derive(Clone)]
pub struct VaultTicker {
    rpc: Arc<RpcClient>,
    subs: Arc<RwLock<HashMap<String, WatchedPool>>>,
    interval: Duration,
    sol_usd: f64,
}

impl VaultTicker {
    /// Build a ticker: `http_url` + commitment for reads, `interval`
    /// between poll rounds, `sol_usd` for conversion.
    pub fn new(
        http_url: &str,
        commitment: CommitmentConfig,
        interval: Duration,
        sol_usd: f64,
    ) -> Result<Self, VaultError> {
        match url::Url::parse(http_url) {
            Ok(u) if u.scheme() == "http" || u.scheme() == "https" => {}
            _ => return Err(VaultError::BadUrl(http_url.to_string())),
        }
        Ok(Self {
            rpc: Arc::new(RpcClient::new_with_commitment(
                http_url.to_string(),
                commitment,
            )),
            subs: Arc::new(RwLock::new(HashMap::new())),
            interval,
            sol_usd: if sol_usd.is_finite() && sol_usd > 0.0 {
                sol_usd
            } else {
                0.0
            },
        })
    }

    /// Watch a pool's vaults (idempotent per mint).
    pub async fn watch(
        &self,
        mint: &str,
        amm_id: &str,
        base_vault: &str,
        quote_vault: &str,
        base_decimals: u8,
        quote_decimals: u8,
    ) -> Result<(), VaultError> {
        let parse =
            |s: &str| Pubkey::from_str(s).map_err(|_| VaultError::BadAddress(s.to_string()));
        let entry = WatchedPool {
            mint: mint.to_string(),
            amm_id: parse(amm_id)?,
            base_vault: parse(base_vault)?,
            quote_vault: parse(quote_vault)?,
            base_decimals,
            quote_decimals,
        };
        self.subs.write().await.insert(mint.to_string(), entry);
        Ok(())
    }

    /// Watch with standard Pump.fun/WSOL decimals (6/9).
    pub async fn watch_defaults(
        &self,
        mint: &str,
        amm_id: &str,
        base_vault: &str,
        quote_vault: &str,
    ) -> Result<(), VaultError> {
        self.watch(
            mint,
            amm_id,
            base_vault,
            quote_vault,
            PUMP_TOKEN_DECIMALS_DEFAULT,
            WSOL_DECIMALS_DEFAULT,
        )
        .await
    }

    /// Stop watching a mint. Returns true when an entry was removed.
    pub async fn unwatch(&self, mint: &str) -> bool {
        self.subs.write().await.remove(mint).is_some()
    }

    /// Watched mint count.
    pub async fn watched_count(&self) -> usize {
        self.subs.read().await.len()
    }

/// Maximum vault pools per `getMultipleAccounts` round chunk.
/// Mirrors the curve ticker's chunking: one oversized request must not
/// fail (or rate-limit) the whole round.
pub const VAULT_BATCH_CHUNK_PAIRS: usize = 50;

/// Poll loop: one batched vault read per round, one tick per pool.
/// Interval-based (no drift); batch failures back off. Ends when the
/// receiver drops.
    pub async fn run(self, tx: mpsc::Sender<PoolTick>) {
        let mut timer = tokio::time::interval(self.interval);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            timer.tick().await;
            if tx.is_closed() {
                debug!("pool tick receiver closed; ticker stopping");
                return;
            }
            let watched: Vec<WatchedPool> = self.subs.read().await.values().cloned().collect();
            if watched.is_empty() {
                continue;
            }
            // Flatten to chunked batched calls: [base0, quote0, ...] per
            // chunk so pool growth cannot breach per-request limits.
            // Chunks preserve pool order; results are concatenated.
            let mut accounts = Vec::with_capacity(watched.len() * 2);
            let mut batch_failed = false;
            for pool_chunk in watched.chunks(Self::VAULT_BATCH_CHUNK_PAIRS.max(1)) {
                let keys: Vec<Pubkey> = pool_chunk
                    .iter()
                    .flat_map(|w| [w.base_vault, w.quote_vault])
                    .collect();
                match tokio::time::timeout(
                    Duration::from_secs(15),
                    self.rpc.get_multiple_accounts(&keys),
                )
                .await
                {
                    Ok(Ok(mut got)) => accounts.append(&mut got),
                    Ok(Err(e)) => {
                        warn!(error = %e, pools = watched.len(), "pool vault batch failed; backing off");
                        batch_failed = true;
                        break;
                    }
                    Err(_) => {
                        warn!(pools = watched.len(), "pool vault batch timed out; backing off");
                        batch_failed = true;
                        break;
                    }
                }
            }
            if batch_failed {
                tokio::time::sleep(self.interval).await;
                continue;
            }
            if accounts.len() != watched.len() * 2 {
                warn!(
                    expected = watched.len() * 2,
                    got = accounts.len(),
                    "vault batch length mismatch; skipping round"
                );
                continue;
            }
            for (pool, pair) in watched.iter().zip(accounts.as_chunks::<2>().0) {
                let (Some(base), Some(quote)) = (&pair[0], &pair[1]) else {
                    debug!(mint = %pool.mint, "pool vault missing");
                    continue;
                };
                let Some(tick) = price_tick(pool, &base.data, &quote.data, self.sol_usd) else {
                    debug!(mint = %pool.mint, "pool price uncomputable");
                    continue;
                };
                if tx.send(tick).await.is_err() {
                    debug!("pool tick receiver closed; ticker stopping");
                    return;
                }
            }
        }
    }
}

/// Current unix seconds (0 on clock failure; never panics).
fn now_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read a raw SPL token amount from account data.
fn token_amount(data: &[u8]) -> Option<u64> {
    data.get(TOKEN_AMOUNT_OFFSET..TOKEN_AMOUNT_OFFSET + 8)
        .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
        .map(u64::from_le_bytes)
}

/// Price one pool from raw vault account data. Pure; unit-tested.
/// f64 ratios hold ~1e-16 relative precision at any magnitude; only the
/// final USD values are range-checked.
fn price_tick(
    pool: &WatchedPool,
    base_data: &[u8],
    quote_data: &[u8],
    sol_usd: f64,
) -> Option<PoolTick> {
    if base_data.len() < TOKEN_ACCOUNT_MIN_LEN || quote_data.len() < TOKEN_ACCOUNT_MIN_LEN {
        return None;
    }
    let base_ui = token_amount(base_data)? as f64 / 10f64.powi(i32::from(pool.base_decimals));
    let quote_ui = token_amount(quote_data)? as f64 / 10f64.powi(i32::from(pool.quote_decimals));
    if !base_ui.is_finite() || base_ui <= 0.0 || !quote_ui.is_finite() || quote_ui < 0.0 {
        return None;
    }
    if !sol_usd.is_finite() || sol_usd <= 0.0 {
        return None;
    }
    let price_usd = quote_ui / base_ui * sol_usd;
    let liquidity_usd = quote_ui * sol_usd;
    if !price_usd.is_finite() || price_usd <= 0.0 || !liquidity_usd.is_finite() {
        return None;
    }
    Some(PoolTick {
        mint: pool.mint.clone(),
        amm_id: pool.amm_id.to_string(),
        price_usd,
        liquidity_usd,
        observed_at: now_secs(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault_bytes(amount: u64) -> Vec<u8> {
        let mut data = vec![0u8; 165];
        data[64..72].copy_from_slice(&amount.to_le_bytes());
        data
    }

    fn pool() -> WatchedPool {
        WatchedPool {
            mint: "MINT".to_string(),
            amm_id: Pubkey::new_from_array([9; 32]),
            base_vault: Pubkey::new_from_array([1; 32]),
            quote_vault: Pubkey::new_from_array([2; 32]),
            base_decimals: 6,
            quote_decimals: 9,
        }
    }

    #[test]
    fn prices_pool_from_vaults() {
        // 1M tokens vs 30 SOL @ $150: $0.0045 per token, $4500 liq.
        let tick = price_tick(
            &pool(),
            &vault_bytes(1_000_000_000_000),
            &vault_bytes(30_000_000_000),
            150.0,
        )
        .expect("tick");
        assert!(
            (tick.price_usd - 0.0045).abs() < 1e-9,
            "price={}",
            tick.price_usd
        );
        assert!((tick.liquidity_usd - 4500.0).abs() < 1e-9);
    }

    #[test]
    fn rejects_degenerate_vaults() {
        assert!(price_tick(&pool(), &[0u8; 10], &vault_bytes(1), 150.0).is_none());
        assert!(price_tick(&pool(), &vault_bytes(0), &vault_bytes(1), 150.0).is_none());
        assert!(price_tick(&pool(), &vault_bytes(1), &vault_bytes(1), 0.0).is_none());
    }

    #[test]
    fn rejects_bad_config() {
        assert!(
            VaultTicker::new(
                "notaurl",
                solana_commitment_config::CommitmentConfig::confirmed(),
                Duration::from_millis(500),
                150.0
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn watch_unwatch_roundtrip() {
        let ticker = VaultTicker::new(
            "https://api.mainnet-beta.solana.com",
            solana_commitment_config::CommitmentConfig::confirmed(),
            Duration::from_millis(500),
            150.0,
        )
        .expect("ticker");
        assert_eq!(ticker.watched_count().await, 0);
        ticker
            .watch_defaults(
                "MINT",
                "11111111111111111111111111111111",
                "11111111111111111111111111111111",
                "11111111111111111111111111111111",
            )
            .await
            .expect("watch");
        assert_eq!(ticker.watched_count().await, 1);
        assert!(ticker.unwatch("MINT").await);
        assert!(!ticker.unwatch("MINT").await);
        assert!(
            ticker
                .watch(
                    "MINT",
                    "11111111111111111111111111111111",
                    "not-a-key",
                    "11111111111111111111111111111111",
                    6,
                    9,
                )
                .await
                .is_err()
        );
    }
}

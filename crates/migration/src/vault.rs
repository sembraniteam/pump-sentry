//! Post-migration pool price ticker over HTTP RPC.
//!
//! Reads both SPL vault balances of each watched Raydium pool in one
//! batched `getMultipleAccounts` round and converts them to USD spot.
//! Pump.fun tokens use 6 decimals, WSOL 9 (overridable per watch).
//! Missed rounds log and continue; the loop ends when the receiver drops.
//!
//! SPL token account layout (only bytes 64..72 are read):
//! `mint(32) owner(32) amount(u64 LE) ...` — 165 bytes total.

use curve::SolUsdFeed;
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
    /// Vault decimals are not plausible (base={0} quote={1}): SPL mints
    /// cap at 18 decimals, so anything above is a misconfiguration that
    /// would misprice the pool silently.
    #[error("invalid vault decimals: base={0} quote={1}")]
    BadConfig(u8, u8),
    /// SOL→USD feed has no fresh price (stale or never fetched).
    #[error("SOL/USD feed stale: no fresh price")]
    StaleSolUsd,
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
    sol_feed: Arc<SolUsdFeed>,
    /// Ticks dropped on a full downstream queue (see `run`).
    dropped: Arc<std::sync::atomic::AtomicU64>,
}

impl VaultTicker {
    /// Build a ticker: `http_url` + commitment for reads, `interval`
    /// between poll rounds, `sol_feed` for SOL→USD (dynamic, stale-gated).
    pub fn new(
        http_url: &str,
        commitment: CommitmentConfig,
        interval: Duration,
        sol_feed: Arc<SolUsdFeed>,
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
            sol_feed,
            dropped: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        })
    }

    /// Ticks dropped on a full downstream queue (backpressure accounting).
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Watch a pool's vaults (idempotent per mint). On Pump.fun migration
    /// pools the base vault holds WSOL (`base_decimals` 9) and the quote
    /// vault holds the token (`quote_decimals` 6) — see `pool` docs.
    pub async fn watch(
        &self,
        mint: &str,
        amm_id: &str,
        base_vault: &str,
        quote_vault: &str,
        base_decimals: u8,
        quote_decimals: u8,
    ) -> Result<(), VaultError> {
        if base_decimals > 18 || quote_decimals > 18 {
            return Err(VaultError::BadConfig(base_decimals, quote_decimals));
        }
        if base_decimals != WSOL_DECIMALS_DEFAULT || quote_decimals != PUMP_TOKEN_DECIMALS_DEFAULT {
            warn!(
                mint = %mint, base_decimals, quote_decimals,
                "non-standard vault decimals (expected 9/6): a 1000x misprice hides here when base/quote are swapped"
            );
        }
        let parse =
            |s: &str| Pubkey::from_str(s).map_err(|_| VaultError::BadAddress(s.to_string()));
        let mint = parse(mint)?.to_string();
        let entry = WatchedPool {
            mint: mint.clone(),
            amm_id: parse(amm_id)?,
            base_vault: parse(base_vault)?,
            quote_vault: parse(quote_vault)?,
            base_decimals,
            quote_decimals,
        };
        self.subs.write().await.insert(mint, entry);
        Ok(())
    }

    /// Watch with standard WSOL/token decimals (9/6): base vault holds
    /// WSOL, quote vault holds the Pump.fun token.
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
            WSOL_DECIMALS_DEFAULT,
            PUMP_TOKEN_DECIMALS_DEFAULT,
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
/// receiver drops. Delivery is drop-on-full (with a counter): one slow
/// consumer must not stall every followed pool.
///
/// Mirrors `curve::CurveTicker::run` on purpose (vault pairs here vs
/// single accounts there, chunked batching): the shared shape is timer
/// → closed-check → snapshot → deadline-bound fetch → length check →
/// parse → drop-on-full send. Keep the backpressure contract (drop +
/// counter + deadlines) in sync across both.
    pub async fn run(self, tx: mpsc::Sender<PoolTick>) {
        let mut timer = tokio::time::interval(self.interval);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Discard the immediate first tick: one full interval before the
        // first round, matching the curve ticker.
        timer.tick().await;
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
            let Some(sol_usd) = self.sol_feed.price() else {
                debug!("sol_usd stale; skipping pool round");
                tokio::time::sleep(self.interval).await;
                continue;
            };
            // Flatten to chunked batched calls: [base0, quote0, ...] per
            // chunk so pool growth cannot breach per-request limits.
            // Chunks preserve pool order; results are concatenated. The
            // whole round is deadline-bound (sequential 15s chunks would
            // otherwise pile missed ticks behind a huge watchlist).
            let accounts = match tokio::time::timeout(
                Duration::from_secs(60),
                self.poll_round(&watched),
            )
            .await
            {
                Ok(accounts) => accounts,
                Err(_) => {
                    warn!(pools = watched.len(), "pool round timed out; backing off");
                    tokio::time::sleep(self.interval).await;
                    continue;
                }
            };
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
                let Some(tick) = price_tick(
                    pool,
                    &base.data,
                    &base.owner,
                    &quote.data,
                    &quote.owner,
                    sol_usd,
                ) else {
                    debug!(mint = %pool.mint, "pool price uncomputable");
                    continue;
                };
                match tx.try_send(tick) {
                    Ok(()) => {}
                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                        self.dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        debug!("pool tick queue full; dropping tick");
                    }
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                        debug!("pool tick receiver closed; ticker stopping");
                        return;
                    }
                }
            }
        }
    }

    /// One round of chunked vault reads (extracted for the round-level
    /// deadline above). A failed chunk aborts the round with a short vec
    /// (the length-mismatch path skips loudly, never half-prices).
    async fn poll_round(&self, watched: &[WatchedPool]) -> Vec<Option<solana_account::Account>> {
        let mut accounts = Vec::with_capacity(watched.len() * 2);
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
                    break;
                }
                Err(_) => {
                    warn!(pools = watched.len(), "pool vault batch timed out; backing off");
                    break;
                }
            }
        }
        accounts
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

/// True for the two token programs whose accounts share the classic
/// 165-byte layout (extensions may extend Token-2022 accounts, never
/// move the mint/owner/amount prefix).
fn is_token_program(owner: &Pubkey) -> bool {
    owner == &ids::TOKEN_PROGRAM_ID || owner == &ids::TOKEN_2022_PROGRAM_ID
}

/// Price one pool from raw vault accounts. Pure; unit-tested.
/// f64 ratios hold ~1e-16 relative precision at any magnitude; only the
/// final USD values are range-checked.
///
/// Orientation (chain-verified, see `pool` docs): on Pump.fun migration
/// pools the BASE vault holds WSOL and the QUOTE vault holds the token.
/// Spot = SOL-per-token (base over quote) × SOL/USD; liquidity = the
/// SOL side in USD.
///
/// Trust gates (fail-closed `None`): both vaults must be owned by a
/// token program (classic SPL or Token-2022 — anything else is not a
/// token account), and the embedded mints must match the watched pair
/// (base vault holds WSOL, quote vault holds the token). A misconfigured
/// watch (swapped vaults, wrong pool) prices the wrong pair silently
/// without these.
fn price_tick(
    pool: &WatchedPool,
    base_data: &[u8],
    base_owner: &Pubkey,
    quote_data: &[u8],
    quote_owner: &Pubkey,
    sol_usd: f64,
) -> Option<PoolTick> {
    if !is_token_program(base_owner) || !is_token_program(quote_owner) {
        return None;
    }
    if base_data.len() < TOKEN_ACCOUNT_MIN_LEN || quote_data.len() < TOKEN_ACCOUNT_MIN_LEN {
        return None;
    }
    let base_mint = Pubkey::try_from(&base_data[0..32]).ok()?;
    if base_mint != ids::WSOL_MINT {
        return None;
    }
    let quote_mint = Pubkey::try_from(&quote_data[0..32]).ok()?;
    if quote_mint.to_string() != pool.mint {
        return None;
    }
    let base_ui = token_amount(base_data)? as f64 / 10f64.powi(i32::from(pool.base_decimals));
    let quote_ui = token_amount(quote_data)? as f64 / 10f64.powi(i32::from(pool.quote_decimals));
    if !base_ui.is_finite() || base_ui <= 0.0 || !quote_ui.is_finite() || quote_ui <= 0.0 {
        return None;
    }
    if !sol_usd.is_finite() || sol_usd <= 0.0 {
        return None;
    }
    let price_usd = base_ui / quote_ui * sol_usd;
    let liquidity_usd = base_ui * sol_usd;
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

    fn vault_bytes(mint: &Pubkey, amount: u64) -> Vec<u8> {
        let mut data = vec![0u8; 165];
        data[0..32].copy_from_slice(&mint.to_bytes());
        data[32..64].copy_from_slice(&[4u8; 32]); // owner placeholder
        data[64..72].copy_from_slice(&amount.to_le_bytes());
        data
    }

    fn token_mint() -> Pubkey {
        Pubkey::new_from_array([5; 32])
    }

    /// Migration orientation: base vault holds WSOL (9dp), quote vault
    /// holds the token (6dp).
    fn pool() -> WatchedPool {
        WatchedPool {
            mint: token_mint().to_string(),
            amm_id: Pubkey::new_from_array([9; 32]),
            base_vault: Pubkey::new_from_array([1; 32]),
            quote_vault: Pubkey::new_from_array([2; 32]),
            base_decimals: 9,
            quote_decimals: 6,
        }
    }

    fn tick(base_sol_lamports: u64, quote_token_units: u64) -> Option<PoolTick> {
        price_tick(
            &pool(),
            &vault_bytes(&ids::WSOL_MINT, base_sol_lamports),
            &ids::TOKEN_PROGRAM_ID,
            &vault_bytes(&token_mint(), quote_token_units),
            &ids::TOKEN_PROGRAM_ID,
            150.0,
        )
    }

    #[test]
    fn prices_pool_from_vaults() {
        // 30 SOL vs 1M tokens @ $150: $0.0045 per token, $4500 liq.
        let tick = tick(30_000_000_000, 1_000_000_000_000).expect("tick");
        assert!(
            (tick.price_usd - 0.0045).abs() < 1e-9,
            "price={}",
            tick.price_usd
        );
        assert!((tick.liquidity_usd - 4500.0).abs() < 1e-9);
    }

    #[test]
    fn rejects_degenerate_vaults() {
        assert!(tick(0, 0).is_none());
        assert!(tick(0, 1).is_none());
        assert!(
            price_tick(
                &pool(),
                &[0u8; 10],
                &ids::TOKEN_PROGRAM_ID,
                &vault_bytes(&token_mint(), 1),
                &ids::TOKEN_PROGRAM_ID,
                150.0,
            )
            .is_none()
        );
        assert!(
            price_tick(
                &pool(),
                &vault_bytes(&ids::WSOL_MINT, 0),
                &ids::TOKEN_PROGRAM_ID,
                &vault_bytes(&token_mint(), 1),
                &ids::TOKEN_PROGRAM_ID,
                150.0,
            )
            .is_none()
        );
        assert!(
            price_tick(
                &pool(),
                &vault_bytes(&ids::WSOL_MINT, 1),
                &ids::TOKEN_PROGRAM_ID,
                &vault_bytes(&token_mint(), 1),
                &ids::TOKEN_PROGRAM_ID,
                0.0,
            )
            .is_none()
        );
    }

    #[test]
    fn rejects_foreign_owner_and_wrong_mints() {
        // Non-token program owner is not a token account.
        let stranger = Pubkey::new_from_array([9; 32]);
        assert!(
            price_tick(
                &pool(),
                &vault_bytes(&ids::WSOL_MINT, 1_000_000),
                &stranger,
                &vault_bytes(&token_mint(), 1_000_000),
                &ids::TOKEN_PROGRAM_ID,
                150.0,
            )
            .is_none()
        );
        // Token-2022 owner is accepted (same layout prefix).
        assert!(
            price_tick(
                &pool(),
                &vault_bytes(&ids::WSOL_MINT, 30_000_000_000),
                &ids::TOKEN_2022_PROGRAM_ID,
                &vault_bytes(&token_mint(), 1_000_000_000_000),
                &ids::TOKEN_2022_PROGRAM_ID,
                150.0,
            )
            .is_some()
        );
        // Base vault not holding WSOL prices the wrong pair.
        assert!(
            price_tick(
                &pool(),
                &vault_bytes(&Pubkey::new_from_array([6; 32]), 1_000_000),
                &ids::TOKEN_PROGRAM_ID,
                &vault_bytes(&token_mint(), 1_000_000),
                &ids::TOKEN_PROGRAM_ID,
                150.0,
            )
            .is_none()
        );
        // Quote vault not holding the followed token is not our pair.
        assert!(
            price_tick(
                &pool(),
                &vault_bytes(&ids::WSOL_MINT, 1_000_000),
                &ids::TOKEN_PROGRAM_ID,
                &vault_bytes(&Pubkey::new_from_array([7; 32]), 1_000_000),
                &ids::TOKEN_PROGRAM_ID,
                150.0,
            )
            .is_none()
        );
    }

    #[test]
    fn rejects_bad_config() {
        use curve::SolUsdFeed;
        assert!(
            VaultTicker::new(
                "notaurl",
                solana_commitment_config::CommitmentConfig::confirmed(),
                Duration::from_millis(500),
                SolUsdFeed::new_for_test(150.0),
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn watch_rejects_implausible_decimals() {
        use curve::SolUsdFeed;
        let ticker = VaultTicker::new(
            "https://api.mainnet-beta.solana.com",
            solana_commitment_config::CommitmentConfig::confirmed(),
            Duration::from_millis(500),
            SolUsdFeed::new_for_test(150.0),
        )
        .expect("ticker");
        let mint = "11111111111111111111111111111111";
        // SPL mints cap at 18 decimals: fail closed, never misprice.
        for (base, quote) in [(19, 6), (9, 19), (u8::MAX, u8::MAX)] {
            let err = ticker
                .watch(mint, mint, mint, mint, base, quote)
                .await
                .expect_err("must fail");
            assert!(matches!(err, VaultError::BadConfig(b, q) if b == base && q == quote));
        }
        assert_eq!(ticker.watched_count().await, 0);
        // Non-standard but plausible decimals (e.g. swapped 6/9) are
        // accepted with a warn, not silently: the caller owns the pair.
        ticker
            .watch(mint, mint, mint, mint, 6, 9)
            .await
            .expect("plausible custom decimals accepted");
        assert_eq!(ticker.watched_count().await, 1);
    }

    #[tokio::test]
    async fn watch_unwatch_roundtrip() {
        use curve::SolUsdFeed;
        let ticker = VaultTicker::new(
            "https://api.mainnet-beta.solana.com",
            solana_commitment_config::CommitmentConfig::confirmed(),
            Duration::from_millis(500),
            SolUsdFeed::new_for_test(150.0),
        )
        .expect("ticker");
        assert_eq!(ticker.watched_count().await, 0);
        ticker
            .watch_defaults(
                "11111111111111111111111111111111",
                "11111111111111111111111111111111",
                "11111111111111111111111111111111",
                "11111111111111111111111111111111",
            )
            .await
            .expect("watch");
        assert_eq!(ticker.watched_count().await, 1);
        assert!(ticker.unwatch("11111111111111111111111111111111").await);
        assert!(!ticker.unwatch("11111111111111111111111111111111").await);
        // Garbage mints fail fast instead of yielding zero ticks forever.
        assert!(
            ticker
                .watch_defaults(
                    "MINT",
                    "11111111111111111111111111111111",
                    "11111111111111111111111111111111",
                    "11111111111111111111111111111111",
                )
                .await
                .is_err()
        );
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

//! PumpSwap pool parser (post-migration venue).
//!
//! Reads the `Pool` account: mints, vaults, signed virtual quote
//! reserves, kept-fee counters, mode flags. Base side holds the
//! migrated token, quote side WSOL on executable pools.
//!
//! Chain-verified against mainnet pool
//! `GeWJCTs9HW5KP7N4mkovNgRjCEszntTb7czmHtXbRrCK`.

use solana_pubkey::Pubkey;

pub use ids::WSOL_MINT;

use crate::detector::{MigrationError, PUMP_AMM_PROGRAM_ID};
use ids::{
    POOL_BASE_MINT_OFFSET, POOL_BASE_VAULT_OFFSET, POOL_CASHBACK_FLAG_OFFSET,
    POOL_COIN_CREATOR_OFFSET, POOL_CREATOR_FEES_OFFSET, POOL_CREATOR_FEE_BPS_OFFSET,
    POOL_HOLDER_REWARD_FLAG_OFFSET, POOL_MAYHEM_FLAG_OFFSET, POOL_MIN_LEN,
    POOL_PROTOCOL_FEES_OFFSET, POOL_QUOTE_MINT_OFFSET, POOL_QUOTE_VAULT_OFFSET, POOL_VIRT_Q_OFFSET,
    PUMP_AMM_POOL_DISCRIMINATOR,
};

/// Migration PumpSwap pool, ready for routing.
/// BASE is the token, QUOTE is WSOL (opposite of Raydium legs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PumpSwapPool {
    /// Pool address (caller-supplied).
    pub pool: Pubkey,
    /// Base mint (migrated token).
    pub base_mint: Pubkey,
    /// Quote mint (WSOL on executable pools).
    pub quote_mint: Pubkey,
    /// Base vault (token inventory).
    pub base_vault: Pubkey,
    /// Quote vault (WSOL inventory).
    pub quote_vault: Pubkey,
    /// Coin creator (default key = no creator fee).
    pub coin_creator: Pubkey,
    /// Pool creator-fee rate bps (0 = standard schedule).
    pub creator_fee_bps: u64,
    /// Signed virtual quote reserves (may be negative).
    pub virtual_quote_reserves: i128,
    /// Kept protocol + creator fees (subset of vault balance).
    pub fee_buckets: u128,
    /// Mayhem pricing mode (1e15 fixed supply for tiers).
    pub is_mayhem: bool,
    /// Cashback coin (v2 sells refuse: 6079).
    pub is_cashback: bool,
    /// Holder-rewards coin (trading unchanged).
    pub is_holder_reward: bool,
}

impl PumpSwapPool {
    /// Parse pool fields. Never panics. Trailing fee counters read 0
    /// on older short layouts; bytes past the IDL layout are ignored.
    /// Reads virtual reserves only: call [`Self::validate`] before routing.
    pub fn parse(pool: &Pubkey, data: &[u8]) -> Result<Self, MigrationError> {
        if data.len() < POOL_MIN_LEN {
            return Err(MigrationError::DataTooShort(data.len()));
        }
        if data[..8] != PUMP_AMM_POOL_DISCRIMINATOR {
            return Err(MigrationError::NotAMigration);
        }
        let slot = |at: usize| -> Pubkey {
            let mut buf = [0u8; 32];
            buf.copy_from_slice(&data[at..at + 32]);
            Pubkey::new_from_array(buf)
        };
        let flag = |at: usize| -> bool { data.get(at).copied().unwrap_or(0) != 0 };
        let u64le = |at: usize| -> u64 {
            data.get(at..at + 8)
                .and_then(|b| <[u8; 8]>::try_from(b).ok())
                .map(u64::from_le_bytes)
                .unwrap_or(0)
        };
        let virt = data
            .get(POOL_VIRT_Q_OFFSET..POOL_VIRT_Q_OFFSET + 16)
            .and_then(|b| <[u8; 16]>::try_from(b).ok())
            .map(i128::from_le_bytes)
            .unwrap_or(0);
        Ok(Self {
            pool: *pool,
            base_mint: slot(POOL_BASE_MINT_OFFSET),
            quote_mint: slot(POOL_QUOTE_MINT_OFFSET),
            base_vault: slot(POOL_BASE_VAULT_OFFSET),
            quote_vault: slot(POOL_QUOTE_VAULT_OFFSET),
            coin_creator: slot(POOL_COIN_CREATOR_OFFSET),
            creator_fee_bps: u64le(POOL_CREATOR_FEE_BPS_OFFSET),
            virtual_quote_reserves: virt,
            fee_buckets: u128::from(u64le(POOL_PROTOCOL_FEES_OFFSET))
                + u128::from(u64le(POOL_CREATOR_FEES_OFFSET)),
            is_mayhem: flag(POOL_MAYHEM_FLAG_OFFSET),
            is_cashback: flag(POOL_CASHBACK_FLAG_OFFSET),
            is_holder_reward: flag(POOL_HOLDER_REWARD_FLAG_OFFSET),
        })
    }

    /// True when quote is WSOL (executable by our sells).
    pub fn is_wsol_quoted(&self) -> bool {
        self.quote_mint == WSOL_MINT
    }

    /// Trust gate: AMM owner, expected base mint, WSOL quote.
    pub fn validate(pool: &PumpSwapPool, owner: &Pubkey, expected_token_mint: &Pubkey) -> bool {
        *owner == PUMP_AMM_PROGRAM_ID
            && pool.base_mint == *expected_token_mint
            && pool.is_wsol_quoted()
    }

    /// Parse plus trust gate (`None` on failure).
    pub fn parse_validated(
        pool: &Pubkey,
        data: &[u8],
        owner: &Pubkey,
        expected_token_mint: &Pubkey,
    ) -> Option<Self> {
        match Self::parse(pool, data) {
            Ok(parsed) if Self::validate(&parsed, owner, expected_token_mint) => Some(parsed),
            _ => None,
        }
    }
}

/// Signed virtual quote reserves from a pool account (effective-quote
/// pricing). `None` on wrong owner, short data, or foreign account.
pub fn virtual_reserves(data: &[u8], owner: &Pubkey) -> Option<i128> {
    if *owner != PUMP_AMM_PROGRAM_ID || data.len() < POOL_MIN_LEN {
        return None;
    }
    if data[..8] != PUMP_AMM_POOL_DISCRIMINATOR {
        return None;
    }
    data.get(POOL_VIRT_Q_OFFSET..POOL_VIRT_Q_OFFSET + 16)
        .and_then(|b| <[u8; 16]>::try_from(b).ok())
        .map(i128::from_le_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    /// Frozen mainnet pool fixture: `{amm, owner, data: [u8]}`.
    fn frozen_pool() -> (Pubkey, Pubkey, Vec<u8>) {
        let raw = include_str!("../tests/fixtures/pumpswap_pool_mainnet.json");
        let v: serde_json::Value = serde_json::from_str(raw).expect("fixture json");
        let parse = |s: &str| {
            Pubkey::from_str(v[s].as_str().expect("pubkey str")).expect("pubkey")
        };
        let data: Vec<u8> = v["data"]
            .as_array()
            .expect("data array")
            .iter()
            .map(|b| b.as_u64().expect("byte") as u8)
            .collect();
        (parse("amm"), parse("owner"), data)
    }

    #[test]
    fn parses_frozen_mainnet_pool() {
        // Live pool created by the frozen `migrate_v2` transaction:
        // AMM-owned, token at BASE, pump-coin quote here (depth-2).
        let (pool, owner, data) = frozen_pool();
        assert_eq!(owner, PUMP_AMM_PROGRAM_ID);
        let parsed = PumpSwapPool::parse(&pool, &data).expect("parse");
        assert_eq!(parsed.pool, pool);
        assert_eq!(
            parsed.base_mint,
            Pubkey::from_str("9stanqrVUsQ2j7KxdEUXCN1EWes2eXi7hSQYJHRBpump").expect("mint")
        );
        assert_eq!(
            parsed.quote_mint,
            Pubkey::from_str("9AvytnUKsLxPxFHFqS6VLxaxt5p6BhYNr53SD2Chpump").expect("mint")
        );
        assert_eq!(
            parsed.base_vault,
            Pubkey::from_str("8Hnv49vZthwDjDqjcY7ka8hwopR1M9cg8RZhxTVSRJpV").expect("vault")
        );
        assert_eq!(
            parsed.quote_vault,
            Pubkey::from_str("WL1jpoyVmy18L4KExk5LNE6Yhb6YyfnkDQQ9etUBuEs").expect("vault")
        );
        assert_eq!(
            parsed.coin_creator,
            Pubkey::from_str("MxYUzJgnkW5NhVdz25f3FGm6amUkzT4ex3gJjNpRPFf").expect("creator")
        );
        assert_eq!(parsed.creator_fee_bps, 100);
        assert_eq!(parsed.virtual_quote_reserves, 987_731_733_625);
        assert_eq!(parsed.fee_buckets, 0);
        assert!(!parsed.is_mayhem);
        assert!(!parsed.is_cashback);
        assert!(parsed.is_holder_reward);
        // Depth-2 quote: not WSOL-quoted, gate refuses.
        assert!(!parsed.is_wsol_quoted());
        assert!(!PumpSwapPool::validate(&parsed, &owner, &parsed.base_mint));
    }

    #[test]
    fn parses_frozen_wsol_executable_pool() {
        // Live WSOL-quoted executable pool frozen from sell_v2 flow
        // (slot ~457428741): parse plus the trust gate pass for its own
        // base mint — the executable counterpart to the depth-2 fixture
        // above, which pins the fail-closed path instead.
        let raw = include_str!("../tests/fixtures/pumpswap_pool_wsol_mainnet.json");
        let v: serde_json::Value = serde_json::from_str(raw).expect("fixture json");
        assert_eq!(v["via"], "getAccountInfo");
        let parse = |s: &str| {
            Pubkey::from_str(v[s].as_str().expect("pubkey str")).expect("pubkey")
        };
        let (pool, owner) = (parse("amm"), parse("owner"));
        let data: Vec<u8> = v["data"]
            .as_array()
            .expect("data array")
            .iter()
            .map(|b| b.as_u64().expect("byte") as u8)
            .collect();
        assert_eq!(owner, PUMP_AMM_PROGRAM_ID);
        let parsed = PumpSwapPool::parse(&pool, &data).expect("parse");
        assert_eq!(parsed.pool, pool);
        assert_eq!(
            parsed.base_mint,
            Pubkey::from_str("E8ZvySLWzHg2oDBPFyeDYzNzVdN7AA4zTC3tUYs9pump").expect("mint")
        );
        assert_eq!(parsed.quote_mint, WSOL_MINT);
        assert_eq!(
            parsed.base_vault,
            Pubkey::from_str("4H5HnFB2S3fSGo5FX7TACgEVBAgN4usoFqVyxfYpzrn1").expect("vault")
        );
        assert_eq!(
            parsed.quote_vault,
            Pubkey::from_str("HGzWWJcVz3bKBhcoepaNJzhEjprDCmHTHcqxsYR5R14").expect("vault")
        );
        assert_eq!(parsed.virtual_quote_reserves, 12131835608);
        assert!(!parsed.is_mayhem);
        assert!(!parsed.is_cashback);
        // Executable: WSOL-quoted and the gate passes for its base mint.
        assert!(parsed.is_wsol_quoted());
        let token = parsed.base_mint;
        assert!(PumpSwapPool::validate(&parsed, &owner, &token));
        assert!(PumpSwapPool::parse_validated(&pool, &data, &owner, &token).is_some());
        // A distinct live pool, not the depth-2 fixture rewritten.
        assert_ne!(pool, frozen_pool().0);
    }

    #[test]
    fn validates_wsol_quoted_pool() {
        let (pool, owner, mut data) = frozen_pool();
        data[POOL_QUOTE_MINT_OFFSET..POOL_QUOTE_MINT_OFFSET + 32]
            .copy_from_slice(&WSOL_MINT.to_bytes());
        let parsed = PumpSwapPool::parse(&pool, &data).expect("parse");
        assert!(parsed.is_wsol_quoted());
        let token = parsed.base_mint;
        assert!(PumpSwapPool::validate(&parsed, &owner, &token));
        assert!(!PumpSwapPool::validate(&parsed, &token, &token));
        assert!(!PumpSwapPool::validate(
            &parsed,
            &owner,
            &Pubkey::new_from_array([99; 32])
        ));
        assert!(PumpSwapPool::parse_validated(&pool, &data, &owner, &token).is_some());
    }

    #[test]
    fn rejects_short_buffers_and_foreign_discriminators() {
        let pool = Pubkey::new_from_array([9; 32]);
        assert_eq!(
            PumpSwapPool::parse(&pool, &[0u8; 100]),
            Err(MigrationError::DataTooShort(100))
        );
        let mut data = vec![0u8; ids::POOL_FULL_LEN];
        assert_eq!(
            PumpSwapPool::parse(&pool, &data),
            Err(MigrationError::NotAMigration)
        );
        data[..8].copy_from_slice(&PUMP_AMM_POOL_DISCRIMINATOR);
        assert!(PumpSwapPool::parse(&pool, &data).is_ok());
    }

    #[test]
    fn virtual_reserves_read_needs_owner_and_shape() {
        use crate::detector::PUMP_AMM_PROGRAM_ID;
        let (_, owner, data) = frozen_pool();
        assert_eq!(owner, PUMP_AMM_PROGRAM_ID);
        assert_eq!(
            virtual_reserves(&data, &owner),
            Some(987_731_733_625)
        );
        // Wrong owner, short data, foreign discriminator: no reserves.
        let stranger = Pubkey::new_from_array([9; 32]);
        assert_eq!(virtual_reserves(&data, &stranger), None);
        assert_eq!(virtual_reserves(&data[..100], &owner), None);
        assert_eq!(virtual_reserves(&vec![0u8; 300], &owner), None);
    }

    #[test]
    fn short_layouts_default_trailing_fees() {        // Older pools end at virtual reserves: counters read 0.
        let (pool, _, data) = frozen_pool();
        let short = &data[..POOL_MIN_LEN];
        let parsed = PumpSwapPool::parse(&pool, short).expect("parse");
        assert_eq!(parsed.virtual_quote_reserves, 987_731_733_625);
        assert_eq!(parsed.fee_buckets, 0);
        assert_eq!(parsed.creator_fee_bps, 0);
    }
}

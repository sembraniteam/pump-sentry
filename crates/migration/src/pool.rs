//! Fresh Raydium AMM V4 pool parser.
//!
//! Reads swap fields from the 752-byte account.
//! Offsets use 32-byte slots from offset 432.
//!
//! Chain-verified: migration pools are WSOL-based.
//! Slot 400 holds WSOL, slot 432 the token.

use solana_pubkey::Pubkey;

pub use ids::WSOL_MINT;

use crate::detector::MigrationError;

/// Full account size in bytes.
pub const RAYDIUM_V4_ACCOUNT_SIZE: usize = 752;
/// Min prefix holding every field we read.
pub const RAYDIUM_V4_MIN_PREFIX: usize = 496;

/// Base-vault slot.
pub const OFF_BASE_VAULT: usize = 336;
/// Quote-vault slot.
pub const OFF_QUOTE_VAULT: usize = 368;
/// Base-mint slot (WSOL on migrations).
pub const OFF_BASE_MINT: usize = 400;
/// Quote-mint slot (migrated token).
pub const OFF_QUOTE_MINT: usize = 432;
/// LP-mint slot.
pub const OFF_LP_MINT: usize = 464;

/// Slot table must fit the account.
const _: () = assert!(OFF_LP_MINT + 32 <= RAYDIUM_V4_ACCOUNT_SIZE);

/// Fresh Raydium pool, ready for routing.
/// On migrations BASE is WSOL, QUOTE the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaydiumV4Pool {
    /// Pool address (caller-supplied).
    pub amm_id: Pubkey,
    /// Base mint (WSOL on migrations).
    pub base_mint: Pubkey,
    /// Quote mint (migrated token).
    pub quote_mint: Pubkey,
    /// Base vault (WSOL inventory).
    pub base_vault: Pubkey,
    /// Quote vault (token inventory).
    pub quote_vault: Pubkey,
    /// LP mint.
    pub lp_mint: Pubkey,
}

impl RaydiumV4Pool {
    /// Parse address slots. Never panics.
    /// Slot reads only. Call [`Self::validate`] before routing.
    pub fn parse(amm_id: &Pubkey, data: &[u8]) -> Result<Self, MigrationError> {
        if data.len() < RAYDIUM_V4_MIN_PREFIX {
            return Err(MigrationError::DataTooShort(data.len()));
        }
        let slot = |at: usize| -> Pubkey {
            let mut buf = [0u8; 32];
            buf.copy_from_slice(&data[at..at + 32]);
            Pubkey::new_from_array(buf)
        };
        Ok(Self {
            amm_id: *amm_id,
            base_vault: slot(OFF_BASE_VAULT),
            quote_vault: slot(OFF_QUOTE_VAULT),
            base_mint: slot(OFF_BASE_MINT),
            quote_mint: slot(OFF_QUOTE_MINT),
            lp_mint: slot(OFF_LP_MINT),
        })
    }

    /// True when base is WSOL.
    pub fn is_wsol_based(&self) -> bool {
        self.base_mint == WSOL_MINT
    }

    /// Trust gate: owner, WSOL base, expected quote.
    pub fn validate(pool: &RaydiumV4Pool, owner: &Pubkey, expected_token_mint: &Pubkey) -> bool {
        use crate::detector::RAYDIUM_V4_PROGRAM_ID;
        *owner == RAYDIUM_V4_PROGRAM_ID
            && pool.is_wsol_based()
            && pool.quote_mint == *expected_token_mint
    }

    /// Parse plus trust gate (`None` on failure).
    pub fn parse_validated(
        amm_id: &Pubkey,
        data: &[u8],
        owner: &Pubkey,
        expected_token_mint: &Pubkey,
    ) -> Option<Self> {
        match Self::parse(amm_id, data) {
            Ok(pool) if Self::validate(&pool, owner, expected_token_mint) => Some(pool),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool_data() -> Vec<u8> {
        let mut data = vec![0u8; RAYDIUM_V4_ACCOUNT_SIZE];
        let fill = |data: &mut Vec<u8>, at: usize, byte: u8| {
            data[at..at + 32].copy_from_slice(&[byte; 32]);
        };
        fill(&mut data, OFF_BASE_VAULT, 11);
        fill(&mut data, OFF_QUOTE_VAULT, 12);
        fill(&mut data, OFF_BASE_MINT, 13);
        fill(&mut data, OFF_QUOTE_MINT, 14);
        // Leave lp mint distinct.
        fill(&mut data, OFF_LP_MINT, 15);
        data
    }

    #[test]
    fn parses_all_slots() {
        let amm_id = Pubkey::new_from_array([9; 32]);
        let pool = RaydiumV4Pool::parse(&amm_id, &pool_data()).expect("parse");
        assert_eq!(pool.amm_id, amm_id);
        assert_eq!(pool.base_vault, Pubkey::new_from_array([11; 32]));
        assert_eq!(pool.quote_vault, Pubkey::new_from_array([12; 32]));
        assert_eq!(pool.base_mint, Pubkey::new_from_array([13; 32]));
        assert_eq!(pool.quote_mint, Pubkey::new_from_array([14; 32]));
        assert_eq!(pool.lp_mint, Pubkey::new_from_array([15; 32]));
    }

    #[test]
    fn slots_match_documented_table() {
        // Contiguous 32-byte slots in documented order; chain-pinned by
        // `parses_frozen_mainnet_pools` below, not just this check.
        assert_eq!(OFF_BASE_VAULT + 32, OFF_QUOTE_VAULT);
        assert_eq!(OFF_QUOTE_VAULT + 32, OFF_BASE_MINT);
        assert_eq!(OFF_BASE_MINT + 32, OFF_QUOTE_MINT);
        assert_eq!(OFF_QUOTE_MINT + 32, OFF_LP_MINT);
        // Table end fits the account (const-checked below, not here:
        // all-constant asserts belong in a const block).
    }

    #[test]
    fn detects_wsol_base() {
        let amm_id = Pubkey::new_from_array([9; 32]);
        let mut data = pool_data();
        data[OFF_BASE_MINT..OFF_BASE_MINT + 32].copy_from_slice(&WSOL_MINT.to_bytes());
        let pool = RaydiumV4Pool::parse(&amm_id, &data).expect("parse");
        assert!(pool.is_wsol_based());

        let plain = RaydiumV4Pool::parse(&amm_id, &pool_data()).expect("parse");
        assert!(!plain.is_wsol_based());
    }

    #[test]
    fn rejects_short_buffers() {
        let amm_id = Pubkey::new_from_array([9; 32]);
        assert_eq!(
            RaydiumV4Pool::parse(&amm_id, &[0u8; 100]),
            Err(MigrationError::DataTooShort(100))
        );
        assert_eq!(
            RaydiumV4Pool::parse(&amm_id, &[]),
            Err(MigrationError::DataTooShort(0))
        );
    }

    #[test]
    fn validate_gates_trust() {
        use crate::detector::RAYDIUM_V4_PROGRAM_ID;
        let amm_id = Pubkey::new_from_array([9; 32]);
        let mut data = pool_data();
        // Migration orientation: WSOL at BASE, token at QUOTE.
        data[OFF_BASE_MINT..OFF_BASE_MINT + 32].copy_from_slice(&WSOL_MINT.to_bytes());
        let token = Pubkey::new_from_array([13; 32]);
        data[OFF_QUOTE_MINT..OFF_QUOTE_MINT + 32].copy_from_slice(&token.to_bytes());
        let pool = RaydiumV4Pool::parse(&amm_id, &data).expect("parse");

        assert!(RaydiumV4Pool::validate(
            &pool,
            &RAYDIUM_V4_PROGRAM_ID,
            &token
        ));
        // Wrong owner.
        assert!(!RaydiumV4Pool::validate(&pool, &token, &token));
        // Wrong token mint.
        assert!(!RaydiumV4Pool::validate(
            &pool,
            &RAYDIUM_V4_PROGRAM_ID,
            &Pubkey::new_from_array([99; 32])
        ));
        // Non-WSOL base.
        let plain = RaydiumV4Pool::parse(&amm_id, &pool_data()).expect("parse");
        assert!(!RaydiumV4Pool::validate(
            &plain,
            &RAYDIUM_V4_PROGRAM_ID,
            &token
        ));
    }

    /// Frozen pool fixture shape: `{amm, owner, data: [u8; 752]}`.
    /// Unknown names panic (never map a typo to the wrong pool).
    fn frozen_pool(name: &str) -> (Pubkey, Pubkey, Vec<u8>) {
        let raw = match name {
            "fartcoin" => include_str!("../tests/fixtures/raydium_pool_fartcoin.json"),
            "mainnet" => include_str!("../tests/fixtures/raydium_pool_mainnet.json"),
            unknown => panic!("unknown pool fixture: {unknown}"),
        };
        let v: serde_json::Value = serde_json::from_str(raw).expect("fixture json");
        let parse = |s: &str| {
            use std::str::FromStr;
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
    fn parses_frozen_fartcoin_migration_pool() {
        // Frozen Oct-2024 Fartcoin migration pool: 752 bytes,
        // Raydium-owned, WSOL at BASE, Fartcoin at QUOTE.
        use crate::detector::RAYDIUM_V4_PROGRAM_ID;
        use std::str::FromStr;
        let (amm, owner, data) = frozen_pool("fartcoin");
        assert_eq!(data.len(), RAYDIUM_V4_ACCOUNT_SIZE);
        assert_eq!(owner, RAYDIUM_V4_PROGRAM_ID);
        let pool = RaydiumV4Pool::parse(&amm, &data).expect("parse");
        assert!(pool.is_wsol_based());
        assert_eq!(
            pool.quote_mint,
            Pubkey::from_str("9BB6NFEcjBCtnNLFko2FqVQBq8HHM13kCyYcdQbgpump").expect("mint")
        );
        // All five slots distinct and non-zero.
        let slots = [
            pool.base_vault,
            pool.quote_vault,
            pool.base_mint,
            pool.quote_mint,
            pool.lp_mint,
        ];
        for (i, a) in slots.iter().enumerate() {
            assert_ne!(*a, Pubkey::default(), "slot {i} zero");
            for b in &slots[i + 1..] {
                assert_ne!(a, b, "aliased slots");
            }
        }
        // Trust gate passes for the migrated mint, rejects others.
        assert!(RaydiumV4Pool::validate(&pool, &owner, &pool.quote_mint));
        assert!(!RaydiumV4Pool::validate(
            &pool,
            &owner,
            &Pubkey::new_from_array([99; 32])
        ));
        assert!(RaydiumV4Pool::parse_validated(&amm, &data, &owner, &pool.quote_mint).is_some());
    }

    #[test]
    fn parses_frozen_pump_token_pool() {
        // Second frozen pool: same WSOL-based orientation.
        use crate::detector::RAYDIUM_V4_PROGRAM_ID;
        use std::str::FromStr;
        let (amm, owner, data) = frozen_pool("mainnet");
        assert_eq!(data.len(), RAYDIUM_V4_ACCOUNT_SIZE);
        assert_eq!(owner, RAYDIUM_V4_PROGRAM_ID);
        let pool = RaydiumV4Pool::parse(&amm, &data).expect("parse");
        assert!(pool.is_wsol_based());
        assert_eq!(
            pool.quote_mint,
            Pubkey::from_str("HyDKNdnhZNVYQMruBevbNsUWruA9STmAQrS4srXApump").expect("mint")
        );
        assert!(RaydiumV4Pool::parse_validated(&amm, &data, &owner, &pool.quote_mint).is_some());
    }
}

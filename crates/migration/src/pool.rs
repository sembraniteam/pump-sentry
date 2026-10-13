//! Fresh Raydium AMM V4 pool account parser.
//!
//! Reads the `LiquidityStateV4` fields a swap needs straight from the
//! 752-byte account: base/quote mints plus both LP vaults. Offsets below
//! follow the canonical contiguous 32-byte slots anchored by the
//! independently verified `quoteMint` memcmp offset 432
//! (datasize 752, market-program 560, swapQuoteIn 664).
//!
//! ORIENTATION (chain-verified 2026-10-11, NOT assumed): Pump.fun
//! migration pools are WSOL-**based** — slot 400 holds WSOL and slot 432
//! holds the migrated token. Proven on two independent frozen pools
//! (`raydium_pool_fartcoin`: the canonical Oct-2024 Fartcoin migration
//! pool, $8M liquidity; `raydium_pool_mainnet`: a fresh pump-token pool):
//! in both, the base vault holds WSOL and the quote vault holds the
//! token (vault mints match the mint slots; Fartcoin vault balances
//! 37,282 SOL vs 24.9M FART reproduce the market's $8.15M liquidity).
//! Every trust gate and price formula in this crate follows that
//! orientation — a WSOL-quoted pool is NOT a Pump migration pool.

use solana_pubkey::Pubkey;

pub use ids::WSOL_MINT;

use crate::detector::MigrationError;

/// Full `LiquidityStateV4` account size in bytes.
pub const RAYDIUM_V4_ACCOUNT_SIZE: usize = 752;
/// Minimum prefix holding every field we read (through `lpMintAddress`).
pub const RAYDIUM_V4_MIN_PREFIX: usize = 496;

/// Byte offset of the base-vault address slot.
pub const OFF_BASE_VAULT: usize = 336;
/// Byte offset of the quote-vault address slot.
pub const OFF_QUOTE_VAULT: usize = 368;
/// Byte offset of the base-mint address slot (WSOL on migrations).
pub const OFF_BASE_MINT: usize = 400;
/// Byte offset of the quote-mint address slot (migrated token).
pub const OFF_QUOTE_MINT: usize = 432;
/// Byte offset of the LP-mint address slot.
pub const OFF_LP_MINT: usize = 464;

/// Compile-time guard: the slot table must fit the account. A
/// transposed offset constant breaks the build here instead of parsing
/// garbage on mainnet.
const _: () = assert!(OFF_LP_MINT + 32 <= RAYDIUM_V4_ACCOUNT_SIZE);

/// Fresh Raydium AMM V4 pool, ready for instant swap routing.
///
/// Orientation follows the on-chain struct: on Pump.fun migration pools
/// the BASE side is WSOL and the QUOTE side is the migrated token
/// (chain-verified, see module docs) — the reverse of the pre-freeze
/// assumption, which silently rejected every real migration pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaydiumV4Pool {
    /// Pool account address (the AMM ID; caller-supplied, not parsed).
    pub amm_id: Pubkey,
    /// Base (coin) mint — WSOL on Pump.fun migrations.
    pub base_mint: Pubkey,
    /// Quote (pc) mint — the migrated token.
    pub quote_mint: Pubkey,
    /// Coin-side LP vault (WSOL inventory on migrations).
    pub base_vault: Pubkey,
    /// Pc-side LP vault (token inventory on migrations).
    pub quote_vault: Pubkey,
    /// LP mint for pool-share accounting.
    pub lp_mint: Pubkey,
}

impl RaydiumV4Pool {
    /// Parse the address slots from raw account data. Bounds-checked;
    /// never panics. Accepts any buffer holding at least the 496-byte
    /// prefix so forward-compatible account growth still parses.
    ///
    /// UNTRUSTED: the returned addresses are just slot reads — they say
    /// nothing about ownership or mints. Never route at this output
    /// without [`Self::validate`] (or use [`Self::parse_validated`).
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

    /// True when the base side is WSOL (standard Pump.fun migration).
    pub fn is_wsol_based(&self) -> bool {
        self.base_mint == WSOL_MINT
    }

    /// Trust gate for freshly resolved pools: the account must be owned
    /// by the Raydium program (kills garbage/misparsed addresses), the
    /// base side must be WSOL, and the quote must be the expected
    /// migrated mint. Pure; call before routing anything at a parsed pool.
    pub fn validate(pool: &RaydiumV4Pool, owner: &Pubkey, expected_token_mint: &Pubkey) -> bool {
        use crate::detector::RAYDIUM_V4_PROGRAM_ID;
        *owner == RAYDIUM_V4_PROGRAM_ID
            && pool.is_wsol_based()
            && pool.quote_mint == *expected_token_mint
    }

    /// Parse plus trust gate in one step: `None` when the buffer is
    /// short, the owner is not the Raydium program, the base is not
    /// WSOL, or the quote mint is unexpected. Prefer this over
    /// `parse` + manual `validate` at every new call site.
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
        // Leave lp mint distinct from fixtures above.
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
        // The five 32-byte slots are contiguous in documented order: a
        // transposed offset constant breaks this before it breaks parsing.
        // Chain-pinned by `parses_frozen_mainnet_pools` below (two real
        // 752-byte accounts), not just by this self-consistency check.
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
        // Wrong owner (spoofed/garbage account).
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
    /// Unknown names panic: silently mapping a typo to the wrong pool
    /// would green-light a test against unintended bytes.
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
        // Canonical Oct-2024 Fartcoin migration pool ($8M liquidity):
        // 752 bytes, Raydium-owned, WSOL at BASE, Fartcoin at QUOTE.
        // Off-test cross-checks: both vaults' mints match the mint slots,
        // and the vault balances (37,282 SOL vs 24.9M FART) reproduce the
        // market's quoted liquidity.
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
        // Second independent pool (fresh pump token): same WSOL-based
        // orientation — the orientation is systematic, not a one-off.
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

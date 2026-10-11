//! Fresh Raydium AMM V4 pool account parser.
//!
//! Reads the `LiquidityStateV4` fields a swap needs straight from the
//! 752-byte account: base/quote mints plus both LP vaults. Offsets below
//! follow the canonical contiguous 32-byte slots anchored by the
//! independently verified `quoteMint` memcmp offset 432
//! (datasize 752, market-program 560, swapQuoteIn 664).
//!
//! Residual: the table is pinned by unit tests but not yet by a frozen
//! mainnet `LiquidityStateV4` sample — capture one before trusting new
//! fields beyond these five.

use solana_pubkey::Pubkey;

pub use ids::WSOL_MINT;

use crate::detector::MigrationError;

/// Full `LiquidityStateV4` account size in bytes.
pub const RAYDIUM_V4_ACCOUNT_SIZE: usize = 752;
/// Minimum prefix holding every field we read (through `lpMintAddress`).
pub const RAYDIUM_V4_MIN_PREFIX: usize = 496;

/// Byte offsets of the 32-byte address slots we parse.
pub const OFF_BASE_VAULT: usize = 336;
/// Byte offsets of the 32-byte address slots we parse.
pub const OFF_QUOTE_VAULT: usize = 368;
/// Byte offsets of the 32-byte address slots we parse.
pub const OFF_BASE_MINT: usize = 400;
/// Byte offsets of the 32-byte address slots we parse.
pub const OFF_QUOTE_MINT: usize = 432;
/// Byte offsets of the 32-byte address slots we parse.
pub const OFF_LP_MINT: usize = 464;

/// Fresh Raydium AMM V4 pool, ready for instant swap routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaydiumV4Pool {
    /// Pool account address (the AMM ID; caller-supplied, not parsed).
    pub amm_id: Pubkey,
    /// Base (coin) mint — the migrated token.
    pub base_mint: Pubkey,
    /// Quote (pc) mint — WSOL on Pump.fun migrations.
    pub quote_mint: Pubkey,
    /// Coin-side LP vault (base inventory).
    pub base_vault: Pubkey,
    /// Pc-side LP vault (quote inventory).
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

    /// True when the quote side is WSOL (standard Pump.fun migration).
    pub fn is_wsol_quoted(&self) -> bool {
        self.quote_mint == WSOL_MINT
    }

    /// Trust gate for freshly resolved pools: the account must be owned
    /// by the Raydium program (kills garbage/misparsed addresses), the
    /// quote side must be WSOL, and the base must be the expected mint.
    /// Pure; call before routing anything at a parsed pool.
    pub fn validate(pool: &RaydiumV4Pool, owner: &Pubkey, expected_base_mint: &Pubkey) -> bool {
        use crate::detector::RAYDIUM_V4_PROGRAM_ID;
        *owner == RAYDIUM_V4_PROGRAM_ID
            && pool.is_wsol_quoted()
            && pool.base_mint == *expected_base_mint
    }

    /// Parse plus trust gate in one step: `None` when the buffer is
    /// short, the owner is not the Raydium program, the quote is not
    /// WSOL, or the base mint is unexpected. Prefer this over
    /// `parse` + manual `validate` at every new call site.
    pub fn parse_validated(
        amm_id: &Pubkey,
        data: &[u8],
        owner: &Pubkey,
        expected_base_mint: &Pubkey,
    ) -> Option<Self> {
        match Self::parse(amm_id, data) {
            Ok(pool) if Self::validate(&pool, owner, expected_base_mint) => Some(pool),
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
        // NOTE: pins code-to-table consistency, NOT table-to-chain — the
        // offsets still want one frozen mainnet LiquidityStateV4 sample.
        assert_eq!(OFF_BASE_VAULT + 32, OFF_QUOTE_VAULT);
        assert_eq!(OFF_QUOTE_VAULT + 32, OFF_BASE_MINT);
        assert_eq!(OFF_BASE_MINT + 32, OFF_QUOTE_MINT);
        assert_eq!(OFF_QUOTE_MINT + 32, OFF_LP_MINT);
        assert!(OFF_LP_MINT + 32 <= RAYDIUM_V4_ACCOUNT_SIZE);
    }

    #[test]
    fn detects_wsol_quote() {
        let amm_id = Pubkey::new_from_array([9; 32]);
        let mut data = pool_data();
        data[OFF_QUOTE_MINT..OFF_QUOTE_MINT + 32].copy_from_slice(&WSOL_MINT.to_bytes());
        let pool = RaydiumV4Pool::parse(&amm_id, &data).expect("parse");
        assert!(pool.is_wsol_quoted());

        let plain = RaydiumV4Pool::parse(&amm_id, &pool_data()).expect("parse");
        assert!(!plain.is_wsol_quoted());
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
        data[OFF_QUOTE_MINT..OFF_QUOTE_MINT + 32].copy_from_slice(&WSOL_MINT.to_bytes());
        let base = Pubkey::new_from_array([13; 32]);
        data[OFF_BASE_MINT..OFF_BASE_MINT + 32].copy_from_slice(&base.to_bytes());
        let pool = RaydiumV4Pool::parse(&amm_id, &data).expect("parse");

        assert!(RaydiumV4Pool::validate(
            &pool,
            &RAYDIUM_V4_PROGRAM_ID,
            &base
        ));
        // Wrong owner (spoofed/garbage account).
        assert!(!RaydiumV4Pool::validate(&pool, &base, &base));
        // Wrong base mint.
        assert!(!RaydiumV4Pool::validate(
            &pool,
            &RAYDIUM_V4_PROGRAM_ID,
            &Pubkey::new_from_array([99; 32])
        ));
        // Non-WSOL quote.
        let plain = RaydiumV4Pool::parse(&amm_id, &pool_data()).expect("parse");
        assert!(!RaydiumV4Pool::validate(
            &plain,
            &RAYDIUM_V4_PROGRAM_ID,
            &base
        ));
    }
}

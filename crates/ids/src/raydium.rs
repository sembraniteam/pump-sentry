//! Raydium program addresses.

use solana_pubkey::Pubkey;

/// Raydium Liquidity Pool V4 program.
pub const RAYDIUM_V4_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");
/// Wrapped SOL mint (standard quote side).
pub const WSOL_MINT: Pubkey =
    Pubkey::from_str_const("So11111111111111111111111111111111111111112");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_match_canonical_strings() {
        assert_eq!(
            RAYDIUM_V4_PROGRAM_ID.to_string(),
            "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8"
        );
        assert_eq!(
            WSOL_MINT.to_string(),
            "So11111111111111111111111111111111111111112"
        );
    }
}

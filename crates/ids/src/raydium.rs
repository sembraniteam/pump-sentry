//! Raydium program addresses.

use solana_pubkey::Pubkey;

/// Raydium Liquidity Pool V4 program.
pub const RAYDIUM_V4_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");
/// Shared V4 vault authority: every pool's coin/pc vault owner,
/// `accounts[2]` on `SwapBaseInV2`. Constant across pools: never derive
/// per `amm_id` (refuted by swap `3Kg6CRL...davX`: derived `CbaWbgUg...urA47wiG`
/// vs on-chain `5Q544fKr...pge4j1`).
pub const RAYDIUM_VAULT_AUTHORITY: Pubkey =
    Pubkey::from_str_const("5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1");
/// Wrapped SOL mint (standard quote side).
pub const WSOL_MINT: Pubkey = Pubkey::from_str_const("So11111111111111111111111111111111111111112");

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
            RAYDIUM_VAULT_AUTHORITY.to_string(),
            "5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1"
        );
        assert_eq!(
            WSOL_MINT.to_string(),
            "So11111111111111111111111111111111111111112"
        );
    }
}

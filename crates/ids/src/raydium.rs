//! Raydium program addresses.

use solana_pubkey::Pubkey;

/// Raydium Liquidity Pool V4 program.
pub const RAYDIUM_V4_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");
/// Shared V4 vault authority (owner of every pool's coin/pc vaults,
/// `accounts[2]` on every direct `SwapBaseInV2`).
///
/// Proven, not assumed: `accounts[2]` of mainnet swap
/// `3Kg6CRLsivED6fFz5fqkrGUeDuYpnoGsGsF6jm1PgY3i46uN7BXec47ZSgvGtL71FG4pz3uK6hbCEbau7Ur5davX`
/// (Fartcoin pool) plus the vault-owner reads on both frozen migration
/// pools (see `raydium_authority_acceptance`). A per-pool PDA derivation
/// was tried first and REFUTED by that probe — this key is constant
/// across pools, so deriving it per `amm_id` builds failing bundles.
pub const RAYDIUM_VAULT_AUTHORITY: Pubkey =
    Pubkey::from_str_const("5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1");
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
            RAYDIUM_VAULT_AUTHORITY.to_string(),
            "5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1"
        );
        assert_eq!(
            WSOL_MINT.to_string(),
            "So11111111111111111111111111111111111111112"
        );
    }
}

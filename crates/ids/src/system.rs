//! System-level program addresses.

use solana_pubkey::Pubkey;

/// SPL Token program.
pub const TOKEN_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
/// SPL Token-2022 program: v2/mayhem mints live here, with DIFFERENT ATA
/// derivation seeds (`[owner, program, mint]`) — always resolve the
/// owning program per mint instead of assuming the classic one.
pub const TOKEN_2022_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
/// SPL Associated Token program.
pub const ASSOCIATED_TOKEN_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
/// System program.
pub const SYSTEM_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("11111111111111111111111111111111");
/// Rent sysvar.
pub const RENT_SYSVAR_ID: Pubkey =
    Pubkey::from_str_const("SysvarRent111111111111111111111111111111111");

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn match_spl_crates() {
        // Cross-check against the SPL crates so version drift in either
        // direction fails loudly here instead of on-chain.
        assert_eq!(TOKEN_PROGRAM_ID.to_bytes(), spl_token::ID.to_bytes());
        assert_eq!(
            ASSOCIATED_TOKEN_PROGRAM_ID.to_bytes(),
            spl_associated_token_account::ID.to_bytes()
        );
        assert_eq!(
            SYSTEM_PROGRAM_ID,
            Pubkey::from_str("11111111111111111111111111111111").expect("const")
        );
    }

    #[test]
    fn token_2022_is_canonical_and_distinct() {
        // No spl-token-2022 dev-dep in this crate; pin the well-known
        // address string so a typo fails here instead of on-chain.
        assert_eq!(
            TOKEN_2022_PROGRAM_ID.to_string(),
            "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"
        );
        assert_ne!(TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID);
    }
}

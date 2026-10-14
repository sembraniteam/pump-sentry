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
/// Compute Budget program (canonical, stable across SDK versions).
/// Single source: `executor` re-exports this instead of defining its own.
pub const COMPUTE_BUDGET_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("ComputeBudget111111111111111111111111111111");
/// Base transaction fee per signature (5_000 lamports, network constant).
/// Single source for live dispatch-cost accounting: every Jito-routed
/// dispatch pays this on top of tip + priority fee, whether or not the
/// bundle lands.
pub const BASE_TX_FEE_LAMPORTS: u64 = 5_000;
/// Rent-exempt reserve for one ATA (165 bytes at the current rent rate).
/// Single source estimate for first-touch cost accounting: creating the
/// user's ATA for a new mint locks this amount (recoverable on close,
/// but a real same-day outflow for the day-loss gate). Revisit only if
/// the rent rate or ATA layout changes — never fork a local copy.
pub const ATA_RENT_LAMPORTS: u64 = 2_039_280;

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

    #[test]
    fn compute_budget_is_canonical() {
        assert_eq!(
            COMPUTE_BUDGET_PROGRAM_ID.to_string(),
            "ComputeBudget111111111111111111111111111111"
        );
    }

    #[test]
    fn fee_and_rent_pins() {
        // Pin the single-source cost constants: any network/rent change
        // must be a deliberate edit here, never silent drift.
        assert_eq!(BASE_TX_FEE_LAMPORTS, 5_000);
        assert_eq!(ATA_RENT_LAMPORTS, 2_039_280);
    }
}

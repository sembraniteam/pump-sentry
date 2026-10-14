//! System-level program addresses.

use solana_pubkey::Pubkey;

/// SPL Token program.
pub const TOKEN_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
/// SPL Token-2022 program: ATA seeds differ (`[owner, program, mint]`).
/// Resolve per mint, never assume classic.
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
/// Compute Budget program. Defined only here; `executor` re-exports.
pub const COMPUTE_BUDGET_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("ComputeBudget111111111111111111111111111111");
/// Base fee per signature (5_000 lamports). Paid on top of tip, landed or not.
pub const BASE_TX_FEE_LAMPORTS: u64 = 5_000;
/// Rent for one ATA (165 bytes). Change only on rent/ATA change.
pub const ATA_RENT_LAMPORTS: u64 = 2_039_280;
/// Basis-point denominator (100%): slippage, fees, pro-rata math.
pub const BPS_DENOMINATOR: u64 = 10_000;

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn match_spl_crates() {
        // Drift must fail here, not on-chain.
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
        // Pin the string so a typo fails here, not on-chain.
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
        // Network/rent changes are deliberate edits here, never silent drift.
        assert_eq!(BASE_TX_FEE_LAMPORTS, 5_000);
        assert_eq!(ATA_RENT_LAMPORTS, 2_039_280);
        assert_eq!(BPS_DENOMINATOR, 10_000);
    }
}

//! Pump.fun buy/sell instruction builders (skill `solana-pumpfun-swap`).
//!
//! Layouts and account order follow the skill exactly. Program addresses
//! are compile-time constants; SPL program IDs cross-check against the
//! `spl-token` / `spl-associated-token-account` crates so version drift
//! fails loudly in tests instead of on-chain.

use borsh::{BorshDeserialize, BorshSerialize};
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use std::str::FromStr;

/// Pump.fun program.
pub const PUMP_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");
/// Fee recipient (writable).
pub const FEE_RECIPIENT: Pubkey =
    Pubkey::from_str_const("CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM");
/// Global config PDA (readonly).
pub const GLOBAL_ACCOUNT: Pubkey =
    Pubkey::from_str_const("4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf");
/// CPI event authority (readonly).
pub const EVENT_AUTHORITY: Pubkey =
    Pubkey::from_str_const("Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1");
/// SPL Token program.
pub const TOKEN_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
/// SPL Associated Token program.
pub const ASSOCIATED_TOKEN_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
/// System program.
pub const SYSTEM_PROGRAM_ID: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");
/// Rent sysvar.
pub const RENT_SYSVAR_ID: Pubkey =
    Pubkey::from_str_const("SysvarRent111111111111111111111111111111111");

/// Buy discriminator: sha256(`global:buy`)[..8].
pub const PUMP_BUY_DISCRIMINATOR: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];
/// Sell discriminator: sha256(`global:sell`)[..8].
pub const PUMP_SELL_DISCRIMINATOR: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];

/// Buy args: token units requested + max SOL spend (slippage guard).
#[derive(BorshSerialize, BorshDeserialize, Debug, PartialEq)]
pub struct PumpBuyArgs {
    /// Token units requested (base units).
    pub amount: u64,
    /// Maximum SOL in lamports allowed (slippage guard).
    pub max_sol_cost: u64,
}

/// Sell args: token units to sell + min SOL out (slippage guard).
#[derive(BorshSerialize, BorshDeserialize, Debug, PartialEq)]
pub struct PumpSellArgs {
    /// Token units to sell (base units).
    pub amount: u64,
    /// Minimum SOL in lamports to receive (slippage guard).
    pub min_sol_output: u64,
}

/// Stateless builder for Pump.fun swap instructions.
pub struct PumpSwapBuilder;

impl PumpSwapBuilder {
    /// Buy instruction: spends SOL for `amount` tokens, capped at
    /// `max_sol_cost` lamports.
    #[allow(clippy::too_many_arguments)]
    pub fn build_buy_ix(
        payer: &Pubkey,
        mint: &Pubkey,
        bonding_curve: &Pubkey,
        associated_bonding_curve: &Pubkey,
        user_ata: &Pubkey,
        amount: u64,
        max_sol_cost: u64,
    ) -> Instruction {
        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&PUMP_BUY_DISCRIMINATOR);
        PumpBuyArgs {
            amount,
            max_sol_cost,
        }
        .serialize(&mut data)
        .expect("in-memory Borsh encode cannot fail");

        let accounts = vec![
            AccountMeta::new_readonly(GLOBAL_ACCOUNT, false),
            AccountMeta::new(FEE_RECIPIENT, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(*bonding_curve, false),
            AccountMeta::new(*associated_bonding_curve, false),
            AccountMeta::new(*user_ata, false),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(RENT_SYSVAR_ID, false),
            AccountMeta::new_readonly(EVENT_AUTHORITY, false),
            AccountMeta::new_readonly(PUMP_PROGRAM_ID, false),
        ];

        Instruction {
            program_id: PUMP_PROGRAM_ID,
            accounts,
            data,
        }
    }

    /// Sell instruction: sells `amount` tokens for at least
    /// `min_sol_output` lamports.
    #[allow(clippy::too_many_arguments)]
    pub fn build_sell_ix(
        payer: &Pubkey,
        mint: &Pubkey,
        bonding_curve: &Pubkey,
        associated_bonding_curve: &Pubkey,
        user_ata: &Pubkey,
        amount: u64,
        min_sol_output: u64,
    ) -> Instruction {
        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&PUMP_SELL_DISCRIMINATOR);
        PumpSellArgs {
            amount,
            min_sol_output,
        }
        .serialize(&mut data)
        .expect("in-memory Borsh encode cannot fail");

        let accounts = vec![
            AccountMeta::new_readonly(GLOBAL_ACCOUNT, false),
            AccountMeta::new(FEE_RECIPIENT, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(*bonding_curve, false),
            AccountMeta::new(*associated_bonding_curve, false),
            AccountMeta::new(*user_ata, false),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
            AccountMeta::new_readonly(EVENT_AUTHORITY, false),
            AccountMeta::new_readonly(PUMP_PROGRAM_ID, false),
        ];

        Instruction {
            program_id: PUMP_PROGRAM_ID,
            accounts,
            data,
        }
    }
}

/// Derive the payer's associated token account for a mint.
///
/// Cross-crate conversion via raw bytes keeps this independent of the
/// `spl-associated-token-account` crate's internal pubkey version.
pub fn user_ata(payer: &Pubkey, mint: &Pubkey) -> Pubkey {
    let theirs = spl_associated_token_account::get_associated_token_address(
        &pubkey_into_theirs(payer),
        &pubkey_into_theirs(mint),
    );
    Pubkey::new_from_array(theirs.to_bytes())
}

fn pubkey_into_theirs(key: &Pubkey) -> solana_pubkey::Pubkey {
    solana_pubkey::Pubkey::new_from_array(key.to_bytes())
}

/// Assert our address constants match the SPL crates (drift detector).
pub fn spl_ids_match() -> bool {
    TOKEN_PROGRAM_ID.to_bytes() == spl_token::ID.to_bytes()
        && ASSOCIATED_TOKEN_PROGRAM_ID.to_bytes() == spl_associated_token_account::ID.to_bytes()
}

/// Parse helper for tests and plumbing.
pub fn parse_pubkey(s: &str) -> Result<Pubkey, solana_pubkey::ParsePubkeyError> {
    Pubkey::from_str(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> (Pubkey, Pubkey, Pubkey, Pubkey, Pubkey) {
        (
            Pubkey::new_from_array([1; 32]),
            Pubkey::new_from_array([2; 32]),
            Pubkey::new_from_array([3; 32]),
            Pubkey::new_from_array([4; 32]),
            Pubkey::new_from_array([5; 32]),
        )
    }

    #[test]
    fn program_addresses_match_skill() {
        assert_eq!(
            PUMP_PROGRAM_ID.to_string(),
            "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P"
        );
        assert_eq!(
            FEE_RECIPIENT.to_string(),
            "CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM"
        );
        assert_eq!(
            GLOBAL_ACCOUNT.to_string(),
            "4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf"
        );
        assert_eq!(
            EVENT_AUTHORITY.to_string(),
            "Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1"
        );
        assert!(spl_ids_match());
    }

    #[test]
    fn buy_layout_and_accounts() {
        let (payer, mint, curve, assoc, ata) = keys();
        let ix = PumpSwapBuilder::build_buy_ix(&payer, &mint, &curve, &assoc, &ata, 1_000, 500);
        assert_eq!(ix.program_id, PUMP_PROGRAM_ID);
        assert_eq!(&ix.data[..8], PUMP_BUY_DISCRIMINATOR);
        assert_eq!(ix.data.len(), 24);
        let args = PumpBuyArgs::try_from_slice(&ix.data[8..]).expect("decode");
        assert_eq!(
            args,
            PumpBuyArgs {
                amount: 1_000,
                max_sol_cost: 500
            }
        );
        assert_eq!(ix.accounts.len(), 12);
        // Spot-check order and mutability per the skill.
        assert_eq!(
            ix.accounts[0],
            AccountMeta::new_readonly(GLOBAL_ACCOUNT, false)
        );
        assert_eq!(ix.accounts[1], AccountMeta::new(FEE_RECIPIENT, false));
        assert_eq!(ix.accounts[6], AccountMeta::new(payer, true));
        assert_eq!(
            ix.accounts[11],
            AccountMeta::new_readonly(PUMP_PROGRAM_ID, false)
        );
        assert!(ix.accounts[6].is_signer);
    }

    #[test]
    fn sell_layout_and_accounts() {
        let (payer, mint, curve, assoc, ata) = keys();
        let ix = PumpSwapBuilder::build_sell_ix(&payer, &mint, &curve, &assoc, &ata, 700, 300);
        assert_eq!(&ix.data[..8], PUMP_SELL_DISCRIMINATOR);
        let args = PumpSellArgs::try_from_slice(&ix.data[8..]).expect("decode");
        assert_eq!(
            args,
            PumpSellArgs {
                amount: 700,
                min_sol_output: 300
            }
        );
        assert_eq!(ix.accounts.len(), 12);
        // Sell carries the ATA program at index 8 (buy carries rent there).
        assert_eq!(
            ix.accounts[8],
            AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM_ID, false)
        );
    }

    #[test]
    fn ata_matches_pda_derivation() {
        let payer = Pubkey::new_from_array([11; 32]);
        let mint = Pubkey::new_from_array([12; 32]);
        let ata = user_ata(&payer, &mint);
        let (expected, _bump) = Pubkey::find_program_address(
            &[
                &payer.to_bytes(),
                &TOKEN_PROGRAM_ID.to_bytes(),
                &mint.to_bytes(),
            ],
            &ASSOCIATED_TOKEN_PROGRAM_ID,
        );
        assert_eq!(ata, expected);
    }
}

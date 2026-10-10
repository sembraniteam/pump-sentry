---
name: solana-pumpfun-swap
description: Binary instruction layout, account schemas, and Rust builder for Pump.fun on-chain Buy and Sell swaps.
license: MIT
compatibility: opencode
metadata:
  domain: solana-dex
  protocol: pump-fun
  language: rust
---

## Purpose

Provides exact binary layouts and strongly typed Rust instruction builders for executing live on-chain `Buy` and `Sell`
swaps against the Pump.fun bonding curve program.

## Constants & Program Addresses

- **Pump.fun Program:** `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P`
- **Fee Recipient:** `CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM`
- **Global Account PDA:** `4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf`
- **Event Authority:** `Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1`
- **Token Program:** `TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA`
- **Associated Token Program:** `ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL`
- **System Program:** `11111111111111111111111111111111`
- **Rent Program:** `SysvarRent111111111111111111111111111111111`

## Instruction Discriminators (Anchor 8-byte SHA256)

- **Buy:** `[102, 6, 61, 18, 1, 218, 235, 234]` (`0x66063d1201daebea`)
- **Sell:** `[51, 230, 133, 164, 1, 127, 131, 173]` (`0x33e685a4017f83ad`)

## Rust Instruction Builder Implementation

```rust
use borsh::{BorshDeserialize, BorshSerialize};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    str::FromStr,
};

pub const PUMP_BUY_DISCRIMINATOR: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];
pub const PUMP_SELL_DISCRIMINATOR: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];

#[derive(BorshSerialize, BorshDeserialize, Debug)]
pub struct PumpBuyArgs {
    pub amount: u64,          // Token units requested
    pub max_sol_cost: u64,    // Maximum SOL in lamports allowed (slippage guard)
}

#[derive(BorshSerialize, BorshDeserialize, Debug)]
pub struct PumpSellArgs {
    pub amount: u64,          // Token units to sell
    pub min_sol_output: u64,  // Minimum SOL in lamports to receive (slippage guard)
}

pub struct PumpSwapBuilder;

impl PumpSwapBuilder {
    pub fn build_buy_ix(
        payer: &Pubkey,
        mint: &Pubkey,
        bonding_curve: &Pubkey,
        associated_bonding_curve: &Pubkey,
        user_ata: &Pubkey,
        amount: u64,
        max_sol_cost: u64,
    ) -> Instruction {
        let program_id = Pubkey::from_str("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P").unwrap();
        let global = Pubkey::from_str("4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf").unwrap();
        let fee_recipient = Pubkey::from_str("CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM").unwrap();
        let token_program = spl_token::id();
        let rent = solana_sdk::sysvar::rent::id();
        let event_authority = Pubkey::from_str("Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1").unwrap();

        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&PUMP_BUY_DISCRIMINATOR);
        PumpBuyArgs { amount, max_sol_cost }.serialize(&mut data).unwrap();

        let accounts = vec![
            AccountMeta::new_readonly(global, false),
            AccountMeta::new(fee_recipient, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(*bonding_curve, false),
            AccountMeta::new(*associated_bonding_curve, false),
            AccountMeta::new(*user_ata, false),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
            AccountMeta::new_readonly(token_program, false),
            AccountMeta::new_readonly(rent, false),
            AccountMeta::new_readonly(event_authority, false),
            AccountMeta::new_readonly(program_id, false),
        ];

        Instruction { program_id, accounts, data }
    }

    pub fn build_sell_ix(
        payer: &Pubkey,
        mint: &Pubkey,
        bonding_curve: &Pubkey,
        associated_bonding_curve: &Pubkey,
        user_ata: &Pubkey,
        amount: u64,
        min_sol_output: u64,
    ) -> Instruction {
        let program_id = Pubkey::from_str("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P").unwrap();
        let global = Pubkey::from_str("4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf").unwrap();
        let fee_recipient = Pubkey::from_str("CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM").unwrap();
        let token_program = spl_token::id();
        let event_authority = Pubkey::from_str("Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1").unwrap();

        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&PUMP_SELL_DISCRIMINATOR);
        PumpSellArgs { amount, min_sol_output }.serialize(&mut data).unwrap();

        let accounts = vec![
            AccountMeta::new_readonly(global, false),
            AccountMeta::new(fee_recipient, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new(*bonding_curve, false),
            AccountMeta::new(*associated_bonding_curve, false),
            AccountMeta::new(*user_ata, false),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
            AccountMeta::new_readonly(spl_associated_token_account::id(), false),
            AccountMeta::new_readonly(token_program, false),
            AccountMeta::new_readonly(event_authority, false),
            AccountMeta::new_readonly(program_id, false),
        ];

        Instruction { program_id, accounts, data }
    }
}
```
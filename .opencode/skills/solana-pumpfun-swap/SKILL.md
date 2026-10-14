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
- **Buy_v2:** `[184, 23, 238, 97, 103, 197, 211, 61]` (mayhem-curve buys; trader at account index 13)
- **Sell_v2:** `[93, 246, 130, 60, 231, 233, 64, 178]` (docs-sourced, UNOBSERVED on-chain — dev dumps through it stay
  invisible; gate fail-closes on unknown pump ixs instead)
- **Create_v2:** `[214, 144, 76, 236, 95, 139, 49, 180]` (current launches; 14–16 accounts)

Buy args carry a trailing plain `bool track_volume` (live 25-byte payloads end `0x00`/`0x01`); the legacy 24-byte
form leaves volume untracked.

## Rust Instruction Builder Implementation

> **DELETED Oct-2026, do not re-add — dispatch is v3-only (17-account
> `buy_v3`/`sell_v3`).** The builder below is the retired legacy shape,
> kept as a parsing/screening reference only. Production dispatch code
> was deleted; see `AGENTS.md` ("Dispatch is v3-only").

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

> **This skill's builder above is the LEGACY shape and was DELETED Oct-2026 — do not re-add; dispatch is
> v3-only (17-account `buy_v3`/`sell_v3`)** (`build_buy_ix` /
> `build_sell_ix` bake in the canonical fee recipient, classic token program, no creator, and untracked volume).
> Production code MUST use `PumpSwapBuilder::build_swap_ix` with explicit `SwapAccounts`
> (`crates/executor/src/instructions.rs`):
>
> - **Fee recipient: resolve live** — explicit override → Global account bytes 41..73 → canonical constant. The
>   program rotates recipients; hardcoding misdirects fees.
> - **Token program: resolve per mint** — classic SPL vs Token-2022 (`TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb`);
>   ATA derivation AND the create-ATA ix must use it.
> - **Creator: enables the extended mayhem shape** — token program, creator vault
>   (`["creator-vault", creator]` PDA), event authority, program, global/user volume accumulators, fee-config
>   accounts (extended BUY byte-verified against mainnet; extended SELL mirrors the SDK order, experimental and
>   simulation-gated). `None` keeps the 12-account legacy core (rejected on mayhem curves).
> - **ATA prefix: idempotent** (`data: [1]`, `CreateIdempotent`) — safe on existing accounts and immune to
>   check-vs-land races.
> - **Buyback accounts [16]/[17]: UNMAPPED** — per-token sourcing is open work; the ignored
>   `mainnet_layout_acceptance` probe is the gate that must turn green before funding.

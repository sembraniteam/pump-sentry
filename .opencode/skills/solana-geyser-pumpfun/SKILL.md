---
name: solana-geyser-pumpfun
description: Binary instruction parsing, account offsets, and Yellowstone gRPC subscription patterns for Pump.fun tokens.
license: MIT
compatibility: opencode
metadata:
  domain: solana-streaming
  language: rust
  target: pump-fun
---

## Purpose

Provides exact binary layout parsing and event extraction for Pump.fun token deployment events in Rust.

## Binary Layout & Offsets

- **Program ID:** `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P`
- **Create Instruction Discriminator (first 8 bytes):** `[24, 30, 200, 40, 5, 28, 7, 119]` (u64 `0x181ec828051c0777`)

## Instruction Parser Implementation

```rust
use borsh::{BorshDeserialize, BorshSerialize};
use solana_sdk::pubkey::Pubkey;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum PumpParseError {
    #[error("Instruction data too short")]
    DataTooShort,
    #[error("Discriminator mismatch")]
    InvalidDiscriminator,
    #[error("Failed to decode Borsh data: {0}")]
    Borsh(#[from] std::io::Error),
    #[error("Insufficient accounts: required at least {required}, found {found}")]
    InsufficientAccounts { required: usize, found: usize },
}

pub const PUMP_CREATE_DISCRIMINATOR: [u8; 8] = [24, 30, 200, 40, 5, 28, 7, 119];

#[derive(BorshDeserialize, BorshSerialize, Debug, Clone)]
pub struct PumpCreateInstructionArgs {
    pub name: String,
    pub symbol: String,
    pub uri: String,
}

#[derive(Debug, Clone)]
pub struct PumpTokenLaunchEvent {
    pub mint: Pubkey,
    pub bonding_curve: Pubkey,
    pub associated_bonding_curve: Pubkey,
    pub user: Pubkey,
    pub name: String,
    pub symbol: String,
    pub uri: String,
}

pub fn parse_pump_create(
    data: &[u8],
    accounts: &[Pubkey],
) -> Result<PumpTokenLaunchEvent, PumpParseError> {
    if data.len() < 8 {
        return Err(PumpParseError::DataTooShort);
    }

    if data[0..8] != PUMP_CREATE_DISCRIMINATOR {
        return Err(PumpParseError::InvalidDiscriminator);
    }

    if accounts.len() < 8 {
        return Err(PumpParseError::InsufficientAccounts {
            required: 8,
            found: accounts.len(),
        });
    }

    let mut cursor = &data[8..];
    let args = PumpCreateInstructionArgs::deserialize(&mut cursor)?;

    Ok(PumpTokenLaunchEvent {
        mint: accounts[0],
        bonding_curve: accounts[2],
        associated_bonding_curve: accounts[3],
        user: accounts[7],
        name: args.name,
        symbol: args.symbol,
        uri: args.uri,
    })
}
```

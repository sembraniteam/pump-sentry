---
name: solana-geyser-pumpfun
description: Binary instruction parsing, account offsets, and create/create_v2 event schemas for Pump.fun tokens.
license: MIT
compatibility: opencode
metadata:
  domain: solana-streaming
  language: rust
  target: pump-fun
---

## Purpose

Provides exact binary layout parsing and event extraction for Pump.fun token deployment events in Rust.

> Transport note: THIS REPO INGESTS VIA `logsSubscribe` WEBSOCKET (`crates/listener`), not Yellowstone gRPC.
> Do not add gRPC plumbing; the parsing below is transport-agnostic and shared by stream and screen paths.

## Binary Layout & Offsets

- **Program ID:** `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P`
- **Create Instruction Discriminator (first 8 bytes):** `[24, 30, 200, 40, 5, 28, 7, 119]` (u64 `0x181ec828051c0777`)
- **Create_v2 Discriminator:** `[214, 144, 76, 236, 95, 139, 49, 180]` (current launches; 14–16 accounts,
  trader-relevant indexes mint=0, curve=2, assoc=3, user=5)
- **Wire-verified create_v2 tail:** string triple, then creator `[32]`, then an 11-byte versioned tail of unknown
  layout (frozen mainnet sample; creator-vault PDA from exactly these bytes matched a live mayhem buy). An IDL-shaped
  struct putting a bool right after the triple would misattribute by one byte — never wire-decode with it.
- **Events carry enrichment:** `signature` + `slot` (+ `creator` for v2, `None` for legacy v1).

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
    #[error("Create args too long: {0} bytes")]
    ArgsTooLong(usize),
    #[error("Create string field is not valid UTF-8")]
    BadString,
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
    pub creator: Option<Pubkey>,
    pub name: String,
    pub symbol: String,
    pub uri: String,
    pub signature: String,
    pub slot: u64,
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
        creator: None,
        name: args.name,
        symbol: args.symbol,
        uri: args.uri,
        signature: String::new(),
        slot: 0,
    })
}
```

## Data-Decoding Rule

RPC instruction `data` arrives as base58 OR base64. A base64 payload using only bs58-alphabet characters mis-decodes
as base58 — so when several decodings succeed, prefer the one opening with a known Pump.fun discriminator
(create/create_v2/buy/sell/buy_v2/sell_v2/migrate); otherwise keep the first success. Single shared helper
(`decode_ix_data`) for every decoder in the crate.

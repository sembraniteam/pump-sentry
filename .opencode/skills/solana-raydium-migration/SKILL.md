---
name: solana-raydium-migration
description: Decodes on-chain Pump.fun curve graduation (offset 48 complete flag), migrate discriminator, and Raydium/PumpSwap pool initialization.
license: MIT
compatibility: opencode
metadata:
  domain: solana-dex
  protocol: raydium-pumpswap
  language: rust
---

## Purpose

Provides verified on-chain account structures, discriminators, and detection routines for identifying when a Pump.fun
bonding curve reaches graduation and migrates liquidity to Raydium AMM V4 or PumpSwap.

## On-Chain Invariants & Constants

### 1. Program Addresses

- **Pump.fun Program:** `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P`
- **Pump Migration Authority:** `39azUYFWPz3VHgKCf3VChUwbpURdCHRxjWVowf5jUJjg`
- **Raydium Liquidity Pool V4:** `675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8`

### 2. Instruction Discriminators & Method Names

- **Pump.fun `migrate` instruction:** `0x9beae792ec9ea21e` (`[155, 234, 231, 146, 236, 158, 162, 30]`).
- **Raydium AMM V4 pool init:** `initialize2` (emitted in log as `Program log: initialize2: InitializeInstruction2`).

### 3. Bonding Curve Graduation Indicator (Binary Offset)

Inside the `BondingCurve` account data (discriminator `0x17b7f83760d8ac60`):

- Offset `32..40`: `real_sol_reserves` (`u64`, lamports).
- Offset `48`: `complete` (`bool`, `1` byte).
    - When `complete == true` (`1`), curve trading on Pump.fun is permanently closed, and the migration transaction is
      triggered.

## Rust Implementation Reference

```rust
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

pub const PUMP_MIGRATE_DISCRIMINATOR: [u8; 8] = [155, 234, 231, 146, 236, 158, 162, 30];
pub const GRADUATION_REAL_SOL_LAMPORTS: u64 = 84_000_000_000; // ~84 SOL

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationDestination {
    RaydiumV4,
    PumpSwap,
    // NOTE: no `Unknown` variant in the repo — unrecognized transactions
    // return `None` (ordinary swaps never match).
}

pub struct MigrationMonitor {
    pump_program: Pubkey,
    pump_migration_authority: Pubkey,
    raydium_v4: Pubkey,
}

impl MigrationMonitor {
    pub fn new() -> Self {
        Self {
            pump_program: Pubkey::from_str("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P").unwrap(),
            pump_migration_authority: Pubkey::from_str("39azUYFWPz3VHgKCf3VChUwbpURdCHRxjWVowf5jUJjg").unwrap(),
            raydium_v4: Pubkey::from_str("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8").unwrap(),
        }
    }

    /// Checks if a bonding curve account has graduated from its raw binary data.
    /// The discriminator is checked FIRST: byte 48 on a non-curve account
    /// (pool, token) must never read as graduation.
    pub fn is_curve_graduated(account_data: &[u8]) -> bool {
        if account_data.len() < 49 {
            return false;
        }
        if account_data[..8] != curve::BONDING_CURVE_DISCRIMINATOR {
            return false;
        }
        account_data[48] != 0 // complete flag at offset 48 (any nonzero byte)
    }

    /// Evaluates graduation progress as percentage (0.0 to 100.0)
    pub fn calculate_progress(real_sol_lamports: u64) -> f64 {
        let progress = (real_sol_lamports as f64 / GRADUATION_REAL_SOL_LAMPORTS as f64) * 100.0;
        progress.clamp(0.0, 100.0)
    }

    /// Detects destination from an on-chain migration transaction.
    /// Requires the watched curve or expected token mint among the keys
    /// (unlinked shared-tx inits must not false-positive), plus:
    pub fn classify_migration(
        &self,
        account_keys: &[Pubkey],
        instructions_data: &[Vec<u8>],
        log_messages: &[String],
        curve: &Pubkey,
        expected_token_mint: &Pubkey,
    ) -> Option<MigrationDestination> {
        let mentions_authority = account_keys.iter().any(|k| k == &self.pump_migration_authority);
        let mentions_pump = account_keys.iter().any(|k| k == &self.pump_program);
        if !mentions_authority && !mentions_pump {
            return None;
        }
        let linked = account_keys.iter().any(|k| k == curve || k == expected_token_mint);
        if !linked {
            return None;
        }

        // 1. Raydium AMM V4 initialization check (ASCII case-insensitive logs)
        let mentions_raydium = account_keys.iter().any(|k| k == &self.raydium_v4);
        let has_raydium_log = log_messages.iter().any(|msg| msg.to_ascii_lowercase().contains("initialize2"));
        if mentions_raydium && has_raydium_log {
            return Some(MigrationDestination::RaydiumV4);
        }

        // 2. PumpSwap migration: legacy instruction OR current MigrateV2 log
        let has_pump_migrate_ix = instructions_data.iter().any(|ix| {
            ix.len() >= 8 && ix[..8] == PUMP_MIGRATE_DISCRIMINATOR
        });
        let has_migrate_v2_log = log_messages.iter().any(|msg| msg.to_ascii_lowercase().contains("migratev2"));
        if has_pump_migrate_ix || has_migrate_v2_log {
            return Some(MigrationDestination::PumpSwap);
        }

        None
    }
}
```
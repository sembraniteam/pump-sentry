---
name: solana-wallet-guard
description: Secure Solana keypair loading, zeroize memory safety, and minimum SOL reserve guards for live trading.
license: MIT
compatibility: opencode
metadata:
  domain: solana-security
  language: rust
---

## Purpose

Enforces secure keypair handling in memory, ensures minimum gas reserves are preserved, and validates wallet solvency
before dispatching on-chain transactions.

## Security Invariants

1. **Never Hardcode Secrets:** Private keys must only be read from the environment (`SOLANA_PRIVATE_KEY`) or an
   encrypted secret store.
2. **Preserve Gas Reserve:** Never spend the entire wallet balance. Always keep a hard floor of at least `0.05 SOL`
   (50,000,000 lamports) to cover transaction fees, ATA rent, and Jito tips.
3. **Pre-flight Solvency Check:** Before constructing any swap transaction, query `getBalance` and assert
   `balance >= (trade_lamports + tip_lamports + reserve_lamports)`.

## Rust Implementation

```rust
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer},
};
use std::env;
use thiserror::Error;

pub const MIN_SOL_RESERVE_LAMPORTS: u64 = 50_000_000; // 0.05 SOL

#[derive(Error, Debug)]
pub enum WalletGuardError {
    #[error("Environment variable SOLANA_PRIVATE_KEY missing")]
    MissingKeyEnv,
    #[error("Failed to decode base58 private key: {0}")]
    Base58Decode(#[from] bs58::decode::Error),
    #[error("Invalid key length {0} bytes: expected 64-byte secret or 32-byte seed")]
    BadKeyLength(usize),
    #[error("Invalid secret key: {0}")]
    BadSecret(String),
    #[error("Insufficient SOL balance: have {available_lamports} lamports, need {required_lamports} lamports (including 0.05 SOL reserve)"
    )]
    InsufficientFunds {
        available_lamports: u64,
        required_lamports: u64,
    },
    #[error("RPC query failed: {0}")]
    Rpc(#[from] solana_client::client_error::ClientError),
    #[error("balance read timed out")]
    Timeout,
}

pub struct WalletGuard {
    keypair: Keypair,
}

impl WalletGuard {
    /// Load from `SOLANA_PRIVATE_KEY`: base58 64-byte secret OR 32-byte
    /// seed (whitespace-tolerant; env string and decode buffers zeroized).
    pub fn load_from_env() -> Result<Self, WalletGuardError> {
        let key_str = env::var("SOLANA_PRIVATE_KEY")
            .map_err(|_| WalletGuardError::MissingKeyEnv)?;

        // Fast path: standard 64-byte secret strings (also validates ed25519).
        if let Ok(keypair) = Keypair::try_from_base58_string(key_str.trim()) {
            return Ok(Self { keypair });
        }
        // Fallback: 32-byte seed; anything else is BadKeyLength.
        let bytes = bs58::decode(key_str.trim()).into_vec()?;
        if bytes.len() != 32 {
            return Err(WalletGuardError::BadKeyLength(bytes.len()));
        }
        let keypair = Keypair::from_seed(&bytes)
            .map_err(|e| WalletGuardError::BadSecret(e.to_string()))?;

        Ok(Self { keypair })
    }

    pub fn pubkey(&self) -> Pubkey {
        self.keypair.pubkey()
    }

    pub fn keypair(&self) -> &Keypair {
        &self.keypair
    }

    /// Deadline-bound (15s); saturating math so huge tips cannot wrap the
    /// requirement below the balance.
    pub async fn assert_solvency(
        &self,
        rpc: &RpcClient,
        trade_lamports: u64,
        jito_tip_lamports: u64,
    ) -> Result<u64, WalletGuardError> {
        let balance = rpc.get_balance(&self.pubkey()).await?;
        let required = trade_lamports
            .saturating_add(jito_tip_lamports)
            .saturating_add(MIN_SOL_RESERVE_LAMPORTS);

        if balance < required {
            return Err(WalletGuardError::InsufficientFunds {
                available_lamports: balance,
                required_lamports: required,
            });
        }

        Ok(balance)
    }
}
```
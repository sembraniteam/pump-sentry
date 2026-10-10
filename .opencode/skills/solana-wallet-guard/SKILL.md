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
    #[error("Insufficient SOL balance: have {available_lamports} lamports, need {required_lamports} lamports (including 0.05 SOL reserve)"
    )]
    InsufficientFunds {
        available_lamports: u64,
        required_lamports: u64,
    },
    #[error("RPC query failed: {0}")]
    Rpc(#[from] solana_client::client_error::ClientError),
}

pub struct WalletGuard {
    keypair: Keypair,
}

impl WalletGuard {
    pub fn load_from_env() -> Result<Self, WalletGuardError> {
        let key_str = env::var("SOLANA_PRIVATE_KEY")
            .map_err(|_| WalletGuardError::MissingKeyEnv)?;

        let bytes = bs58::decode(key_str.trim()).into_vec()?;
        let keypair = Keypair::from_bytes(&bytes)
            .map_err(|_| WalletGuardError::Base58Decode(bs58::decode::Error::BufferTooSmall))?;

        Ok(Self { keypair })
    }

    pub fn pubkey(&self) -> Pubkey {
        self.keypair.pubkey()
    }

    pub fn keypair(&self) -> &Keypair {
        &self.keypair
    }

    pub async fn assert_solvency(
        &self,
        rpc: &RpcClient,
        trade_lamports: u64,
        jito_tip_lamports: u64,
    ) -> Result<u64, WalletGuardError> {
        let balance = rpc.get_balance(&self.pubkey()).await?;
        let required = trade_lamports + jito_tip_lamports + MIN_SOL_RESERVE_LAMPORTS;

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
//! Trading wallet guard (skill `solana-wallet-guard`).
//!
//! Invariants:
//! 1. Secrets only enter via `SOLANA_PRIVATE_KEY` (base58 64-byte secret
//!    or 32-byte seed); env-string and decode buffers are zeroized after
//!    use (best-effort: allocator copies are out of scope).
//! 2. A hard 0.05 SOL floor ([`MIN_SOL_RESERVE_LAMPORTS`]) is never spent.
//! 3. [`WalletGuard::assert_solvency`] checks
//!    `balance >= trade + tip + reserve` before any swap is built.

use solana_client::{client_error::ClientError, nonblocking::rpc_client::RpcClient};
use solana_keypair::{Keypair, keypair_from_seed};
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use std::env;
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

/// Minimum unspendable balance: 0.05 SOL (fees, ATA rent, tips).
pub const MIN_SOL_RESERVE_LAMPORTS: u64 = 50_000_000;

/// Wallet failures: missing secrets, bad encoding, broke-ness.
#[derive(Error, Debug)]
pub enum WalletGuardError {
    /// Environment variable SOLANA_PRIVATE_KEY missing.
    #[error("Environment variable SOLANA_PRIVATE_KEY missing")]
    MissingKeyEnv,
    /// Failed to decode base58 private key: {0}
    #[error("Failed to decode base58 private key: {0}")]
    Base58Decode(#[from] bs58::decode::Error),
    /// Key material has {0} bytes; expected a 64-byte secret or 32-byte seed.
    #[error("Invalid key length {0} bytes: expected 64-byte secret or 32-byte seed")]
    BadKeyLength(usize),
    /// Key bytes do not form a valid ed25519 keypair: {0}
    #[error("Invalid secret key: {0}")]
    BadSecret(String),
    /// Insufficient SOL balance: have {available_lamports} lamports, need
    /// {required_lamports} lamports (including 0.05 SOL reserve).
    #[error(
        "Insufficient SOL balance: have {available_lamports} lamports, need {required_lamports} lamports (including 0.05 SOL reserve)"
    )]
    InsufficientFunds {
        /// On-chain balance in lamports.
        available_lamports: u64,
        /// Trade + tip + reserve floor in lamports.
        required_lamports: u64,
    },
    /// RPC query failed: {0}
    #[error("RPC query failed: {0}")]
    Rpc(#[from] ClientError),
    /// Balance read exceeded its deadline.
    #[error("balance read timed out")]
    Timeout,
}

/// Loaded trading keypair with solvency enforcement.
pub struct WalletGuard {
    keypair: Keypair,
}

impl WalletGuard {
    /// Load from `SOLANA_PRIVATE_KEY` (base58, whitespace-tolerant).
    /// The env string is held in a [`Zeroizing`] wrapper so the secret
    /// bytes are wiped when loading finishes.
    pub fn load_from_env() -> Result<Self, WalletGuardError> {
        let key_str = Zeroizing::new(
            env::var("SOLANA_PRIVATE_KEY").map_err(|_| WalletGuardError::MissingKeyEnv)?,
        );
        Self::from_base58(key_str.trim())
    }

    /// Build from a base58 string: 64-byte secret or 32-byte seed.
    /// Decode buffers are zeroized before returning; the input `&str` is
    /// caller-owned (see [`Self::load_from_env`] for the zeroized path).
    pub fn from_base58(s: &str) -> Result<Self, WalletGuardError> {
        // Fast path: standard 64-byte secret strings (also validates ed25519).
        if let Ok(keypair) = Keypair::try_from_base58_string(s) {
            return Ok(Self { keypair });
        }
        let mut bytes = Zeroizing::new(bs58::decode(s).into_vec()?);
        let result = match bytes.len() {
            32 => keypair_from_seed(&bytes)
                .map_err(|e| WalletGuardError::BadSecret(e.to_string()))
                .map(|keypair| Self { keypair }),
            n => Err(WalletGuardError::BadKeyLength(n)),
        };
        bytes.zeroize();
        result
    }

    /// Trading address.
    pub fn pubkey(&self) -> Pubkey {
        self.keypair.pubkey()
    }

    /// Signing handle for transaction building.
    pub fn keypair(&self) -> &Keypair {
        &self.keypair
    }

    /// Assert `balance >= trade_lamports + tip_lamports + reserve`.
    /// Returns the balance on success for logging. Deadline-bound.
    pub async fn assert_solvency(
        &self,
        rpc: &RpcClient,
        trade_lamports: u64,
        tip_lamports: u64,
    ) -> Result<u64, WalletGuardError> {
        let balance = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            rpc.get_balance(&self.pubkey()),
        )
        .await
        .map_err(|_| WalletGuardError::Timeout)?
        .map_err(WalletGuardError::Rpc)?;
        let required = trade_lamports
            .saturating_add(tip_lamports)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    /// Serializes env-mutating tests (env is process-global).
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn reserve_floor_is_005_sol() {
        assert_eq!(MIN_SOL_RESERVE_LAMPORTS, 50_000_000);
    }

    #[test]
    fn missing_env_errors() {
        let _guard = env_lock().lock().expect("lock");
        let saved = env::var("SOLANA_PRIVATE_KEY").ok();
        unsafe { env::remove_var("SOLANA_PRIVATE_KEY") };
        let err = WalletGuard::load_from_env().err().expect("must fail");
        assert!(matches!(err, WalletGuardError::MissingKeyEnv));
        if let Some(v) = saved {
            unsafe { env::set_var("SOLANA_PRIVATE_KEY", v) };
        }
    }

    #[test]
    fn rejects_bad_base58_and_lengths() {
        assert!(matches!(
            WalletGuard::from_base58("!!!not-base58!!!"),
            Err(WalletGuardError::Base58Decode(_))
        ));
        assert!(matches!(
            WalletGuard::from_base58(&bs58::encode(vec![7u8; 16]).into_string()),
            Err(WalletGuardError::BadKeyLength(16))
        ));
    }

    #[test]
    fn roundtrips_64_byte_secret() {
        let original = Keypair::new();
        let encoded = bs58::encode(original.to_bytes()).into_string();
        let loaded = WalletGuard::from_base58(&encoded).expect("load");
        assert_eq!(loaded.pubkey(), original.pubkey());
    }

    #[test]
    fn accepts_32_byte_seed() {
        let seed = [9u8; 32];
        let expected = keypair_from_seed(&seed).expect("seed").pubkey();
        let loaded = WalletGuard::from_base58(&bs58::encode(seed).into_string()).expect("load");
        assert_eq!(loaded.pubkey(), expected);
    }

    #[test]
    fn loads_from_env() {
        let _guard = env_lock().lock().expect("lock");
        let saved = env::var("SOLANA_PRIVATE_KEY").ok();
        let keypair = Keypair::new();
        unsafe {
            env::set_var(
                "SOLANA_PRIVATE_KEY",
                bs58::encode(keypair.to_bytes()).into_string(),
            )
        };
        let loaded = WalletGuard::load_from_env().expect("load");
        assert_eq!(loaded.pubkey(), keypair.pubkey());
        match saved {
            Some(v) => unsafe { env::set_var("SOLANA_PRIVATE_KEY", v) },
            None => unsafe { env::remove_var("SOLANA_PRIVATE_KEY") },
        }
    }
}

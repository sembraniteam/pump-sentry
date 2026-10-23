//! Trading wallet guard.
//!
//! Secrets enter only via `SOLANA_PRIVATE_KEY` (base58 64-byte secret or
//! 32-byte seed); buffers are zeroized. A 0.05 SOL floor is never spent.

use solana_client::{client_error::ClientError, nonblocking::rpc_client::RpcClient};
use solana_keypair::{Keypair, keypair_from_seed};
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use std::env;
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

/// Unspendable floor: 0.05 SOL (fees, rent, tips).
pub const MIN_SOL_RESERVE_LAMPORTS: u64 = 50_000_000;

/// Wallet failures.
#[derive(Error, Debug)]
pub enum WalletGuardError {
    #[error("SOLANA_PRIVATE_KEY missing (or SOLANA_PRIVATE_KEY_FILE unreadable)")]
    MissingKeyEnv,
    #[error("Failed to decode base58 private key: {0}")]
    Base58Decode(#[from] bs58::decode::Error),
    /// Got {0} bytes; want 64-byte secret or 32-byte seed.
    #[error("Invalid key length {0} bytes: expected 64-byte secret or 32-byte seed")]
    BadKeyLength(usize),
    #[error("Invalid secret key: {0}")]
    BadSecret(String),
    /// Have {available_lamports}, need {required_lamports} (with reserve).
    #[error(
        "Insufficient SOL balance: have {available_lamports} lamports, need {required_lamports} lamports (including 0.05 SOL reserve)"
    )]
    InsufficientFunds {
        available_lamports: u64,
        required_lamports: u64,
    },
    /// Exit refuse: every shortfall leg is named so an operator sees
    /// which knob blocked the exit (tip / priority / fee / ATA rent /
    /// rent-exempt floor). Buys report [`WalletGuardError::InsufficientFunds`]
    /// with the 0.05 reserve instead — this variant is sell-only and
    /// must never gate a buy.
    #[error(
        "Insufficient SOL to exit: have {available_lamports} lamports, need {required_lamports} = tip {tip_lamports} + priority {priority_lamports} + base fee {fee_lamports} + ATA rent {ata_rent_lamports} + rent-exempt floor {floor_lamports}"
    )]
    SellInsufficientFunds {
        available_lamports: u64,
        required_lamports: u64,
        /// Jito tip left on this dispatch (0 on plain sends).
        tip_lamports: u64,
        /// Compute-budget priority fee for this dispatch.
        priority_lamports: u64,
        /// Base per-signature fee (`ids::BASE_TX_FEE_LAMPORTS`).
        fee_lamports: u64,
        /// One ATA rent when the route creates the destination WSOL
        /// ATA, else 0 (the curve v3 sell returns proceeds as native
        /// SOL to the payer and creates no account).
        ata_rent_lamports: u64,
        /// Rent-exempt minimum the payer must hold after the exit
        /// (`ids::SYSTEM_ACCT_RENT_EXEMPT_LAMPORTS`).
        floor_lamports: u64,
    },
    #[error("RPC query failed: {0}")]
    Rpc(#[from] ClientError),
    #[error("balance read timed out")]
    Timeout,
    /// Hot-wallet ceiling breached: sweep before entries can resume.
    #[error(
        "hot wallet holds {available_lamports} lamports, over the {cap_lamports}-lamport cap (HOT_WALLET_MAX_SOL): sweep the excess — entries refuse, exits are unaffected"
    )]
    HotWalletOverCap {
        available_lamports: u64,
        cap_lamports: u64,
    },
    /// Key file readable by group/other: anyone on the host can move
    /// the funds. Refused, never warned past.
    #[error(
        "private key file {path} is {mode:o} (group/other readable); fix with `chmod 600 {path}` before running live"
    )]
    InsecureKeyFile { path: String, mode: u32 },
}

/// Loaded keypair with solvency enforcement.
pub struct WalletGuard {
    keypair: Keypair,
    /// Hot-wallet ceiling in lamports (`0` = off). Entries refuse while
    /// the wallet holds more; exits never consult it.
    hot_wallet_cap_lamports: u64,
}

/// Refuse a key file readable by group/other. `0600` (or stricter) is
/// the only safe mode for a file that moves money; symlinks resolve
/// (`metadata` follows them). Unreadable metadata defers to the read,
/// which reports `MissingKeyEnv`.
#[cfg(unix)]
fn check_key_file_permissions(path: &std::path::Path) -> Result<(), WalletGuardError> {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::metadata(path) else {
        return Ok(());
    };
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(WalletGuardError::InsecureKeyFile {
            path: path.display().to_string(),
            mode,
        });
    }
    Ok(())
}

/// Non-unix: no portable mode bits to enforce.
#[cfg(not(unix))]
fn check_key_file_permissions(_path: &std::path::Path) -> Result<(), WalletGuardError> {
    Ok(())
}

impl WalletGuard {
    /// Load from `SOLANA_PRIVATE_KEY` (trims whitespace, zeroizes),
    /// else from the file at `SOLANA_PRIVATE_KEY_FILE` (preferred:
    /// never `export` the key into shell/profile history — a file
    /// keeps it out of `/proc` environ and `ps`).
    /// On success the variable is scrubbed from the process
    /// environment: the keypair lives only in guarded memory
    /// afterwards, not in `/proc` environ, core dumps, or children
    /// spawned later. A second load fails loudly (`MissingKeyEnv`) —
    /// that is the guarantee working, not a bug.
    pub fn load_from_env() -> Result<Self, WalletGuardError> {
        let key_str = match env::var("SOLANA_PRIVATE_KEY") {
            Ok(k) => Zeroizing::new(k),
            Err(_) => {
                let Some(path) = env::var("SOLANA_PRIVATE_KEY_FILE").ok() else {
                    return Err(WalletGuardError::MissingKeyEnv);
                };
                // A key file the group/world can read is not a secret
                // any more: fail closed before the bytes are loaded.
                check_key_file_permissions(std::path::Path::new(&path))?;
                let Some(k) = std::fs::read_to_string(&path).ok() else {
                    return Err(WalletGuardError::MissingKeyEnv);
                };
                Zeroizing::new(k)
            }
        };
        let loaded = Self::from_base58(key_str.trim())?;
        unsafe { env::remove_var("SOLANA_PRIVATE_KEY") };
        Ok(loaded)
    }

    /// Arm the hot-wallet ceiling (lamports, `0` = off): buy preflights
    /// refuse while the wallet holds more than this, so an un-swept
    /// wallet or a leaked key can only lose the cap.
    pub fn with_hot_wallet_cap(mut self, cap_lamports: u64) -> Self {
        self.hot_wallet_cap_lamports = cap_lamports;
        self
    }

    /// Refuse an *entry* when the wallet is over its ceiling. Pure:
    /// takes the balance the buy preflight already read. Exits never
    /// call this — a fat wallet must not block an exit.
    pub fn check_hot_wallet_cap(&self, balance_lamports: u64) -> Result<(), WalletGuardError> {
        let cap = self.hot_wallet_cap_lamports;
        if cap > 0 && balance_lamports > cap {
            return Err(WalletGuardError::HotWalletOverCap {
                available_lamports: balance_lamports,
                cap_lamports: cap,
            });
        }
        Ok(())
    }

    /// Build from base58: 64-byte secret or 32-byte seed.
    pub fn from_base58(s: &str) -> Result<Self, WalletGuardError> {
        let mut bytes = Zeroizing::new(bs58::decode(s).into_vec()?);
        let result = match bytes.len() {
            64 => Keypair::try_from(bytes.as_slice())
                .map_err(|e| WalletGuardError::BadSecret(e.to_string()))
                .map(|keypair| Self {
                    keypair,
                    hot_wallet_cap_lamports: 0,
                }),
            32 => keypair_from_seed(&bytes)
                .map_err(|e| WalletGuardError::BadSecret(e.to_string()))
                .map(|keypair| Self {
                    keypair,
                    hot_wallet_cap_lamports: 0,
                }),
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

    /// Assert `balance >= trade + tip + priority + base fee + one ATA
    /// rent + reserve`. Deadline-bound.
    /// Priority is explicit (not folded into tip) so shortfalls name the
    /// right knob. Simulation still gates every dispatch; this only
    /// fails fast before burning sim budget.
    pub async fn assert_solvency(
        &self,
        rpc: &RpcClient,
        trade_lamports: u64,
        tip_lamports: u64,
        priority_lamports: u64,
    ) -> Result<u64, WalletGuardError> {
        let balance = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            rpc.get_balance(&self.pubkey()),
        )
        .await
        .map_err(|_| WalletGuardError::Timeout)?
        .map_err(WalletGuardError::Rpc)?;
        // Same legs the funding gate scales (minus the book multiple:
        // this guards one dispatch, the gate guards the book).
        let required = trade_lamports
            .saturating_add(tip_lamports)
            .saturating_add(priority_lamports)
            .saturating_add(ids::BASE_TX_FEE_LAMPORTS)
            .saturating_add(ids::ATA_RENT_LAMPORTS)
            .saturating_add(MIN_SOL_RESERVE_LAMPORTS);
        if balance < required {
            return Err(WalletGuardError::InsufficientFunds {
                available_lamports: balance,
                required_lamports: required,
            });
        }
        Ok(balance)
    }

    /// Sell-side solvency: `tip + priority + base fee` (`+` one ATA
    /// rent only when the route creates the destination WSOL ATA), and
    /// a hard floor so the payer stays rent-exempt after the
    /// transaction. Deliberately **not** [`WalletGuard::assert_solvency`]:
    /// the 0.05 SOL reserve is a *buy* funding rule — a sale funds its
    /// own fee legs atomically from the swap proceeds (Jito bundles
    /// them into one transaction), so applying the reserve to an exit
    /// would strand a drained wallet's bag ("exits never block").
    /// `needs_ata`: Raydium/PumpSwap legs create the destination WSOL
    /// ATA when missing (rent is genuinely consumed there); the curve
    /// v3 sell returns proceeds as native SOL to the payer and creates
    /// no account. Deadline-bound, same read as buys.
    pub async fn assert_sell_solvency(
        &self,
        rpc: &RpcClient,
        tip_lamports: u64,
        priority_lamports: u64,
        needs_ata: bool,
    ) -> Result<u64, WalletGuardError> {
        let balance = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            rpc.get_balance(&self.pubkey()),
        )
        .await
        .map_err(|_| WalletGuardError::Timeout)?
        .map_err(WalletGuardError::Rpc)?;
        let floor = ids::SYSTEM_ACCT_RENT_EXEMPT_LAMPORTS;
        let fee_lamports = ids::BASE_TX_FEE_LAMPORTS;
        let ata_rent_lamports = if needs_ata { ids::ATA_RENT_LAMPORTS } else { 0 };
        let required = tip_lamports
            .saturating_add(priority_lamports)
            .saturating_add(fee_lamports)
            .saturating_add(ata_rent_lamports)
            .saturating_add(floor);
        if balance < required {
            return Err(WalletGuardError::SellInsufficientFunds {
                available_lamports: balance,
                required_lamports: required,
                tip_lamports,
                priority_lamports,
                fee_lamports,
                ata_rent_lamports,
                floor_lamports: floor,
            });
        }
        Ok(balance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    /// Serializes env-mutating tests (process-global env). This is a
    /// separate test binary from the root crate, so a shared lock would
    /// not help there; cross-binary isolation relies on `cargo test`
    /// running test binaries sequentially (AUD-022).
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn reserve_floor_is_005_sol() {
        assert_eq!(MIN_SOL_RESERVE_LAMPORTS, 50_000_000);
    }

    /// Nonblocking client over the solana mock sender: `getBalance`
    /// responses pop per-method queues in insertion order, so each
    /// test dials its own balance(s). No network, no wallet state.
    fn mock_rpc(balances: &[u64]) -> RpcClient {
        use solana_client::{rpc_client::RpcClientConfig, rpc_request::RpcRequest};
        use solana_rpc_client::mock_sender::{MockSender, MocksMap};
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct MockCtx {
            slot: u64,
            api_version: &'static str,
        }
        #[derive(serde::Serialize)]
        struct MockResp<T: serde::Serialize> {
            context: MockCtx,
            value: T,
        }
        let mut mocks = MocksMap::default();
        for balance in balances {
            let resp = MockResp {
                context: MockCtx {
                    slot: 1,
                    api_version: "4.3.0",
                },
                value: *balance,
            };
            mocks.insert(
                RpcRequest::GetBalance,
                serde_json::to_value(resp).expect("mock response serializes"),
            );
        }
        let sender = MockSender::new_with_mocks_map("wallet-solvency", mocks);
        RpcClient::new_sender(
            sender,
            RpcClientConfig::with_commitment(
                solana_commitment_config::CommitmentConfig::confirmed(),
            ),
        )
    }

    fn test_wallet() -> WalletGuard {
        WalletGuard::from_base58(&bs58::encode(Keypair::new().to_bytes()).into_string())
            .expect("wallet")
    }

    /// 0.04 SOL covers tip + priority + base fee and keeps
    /// the payer rent-exempt after the exit, but sits far below the
    /// 0.05 SOL buy reserve. A sell must pass: exits never block on a
    /// buy funding rule. Fails at HEAD.
    #[tokio::test]
    async fn sell_solvency_passes_below_buy_reserve() {
        let guard = test_wallet();
        let rpc = mock_rpc(&[40_000_000]);
        let balance = guard
            .assert_sell_solvency(&rpc, 100_000, 0, false)
            .await
            .expect("sell passes on fee legs alone");
        assert_eq!(balance, 40_000_000);
    }

    /// A wallet that cannot even cover tip + priority + fee
    /// (here 100_000 < 995_880) still refuses — with every shortfall
    /// leg named so the operator sees why the exit was rejected.
    /// Independently derived floor: 100_000 + 0 + 5_000 + 890_880.
    #[tokio::test]
    async fn sell_solvency_refuses_below_fee_legs_with_named_legs() {
        let guard = test_wallet();
        let rpc = mock_rpc(&[100_000]);
        match guard.assert_sell_solvency(&rpc, 100_000, 0, false).await {
            Err(WalletGuardError::SellInsufficientFunds {
                available_lamports,
                required_lamports,
                tip_lamports,
                priority_lamports,
                fee_lamports,
                ata_rent_lamports,
                floor_lamports,
            }) => {
                assert_eq!(available_lamports, 100_000);
                assert_eq!(
                    required_lamports,
                    tip_lamports
                        + priority_lamports
                        + fee_lamports
                        + ata_rent_lamports
                        + floor_lamports
                );
                assert_eq!(required_lamports, 995_880);
                assert_eq!(fee_lamports, 5_000);
                assert_eq!(ata_rent_lamports, 0, "curve sells create no account");
                assert_eq!(floor_lamports, 890_880);
            }
            other => panic!("expected named sell refusal, got {other:?}"),
        }
        // The rendered error (what operators read) must name the legs.
        let rendered = match guard
            .assert_sell_solvency(&mock_rpc(&[100_000]), 100_000, 0, false)
            .await
        {
            Err(e) => e.to_string(),
            Ok(_) => panic!("must refuse"),
        };
        assert!(
            rendered.contains("tip 100000") && rendered.contains("fee 5000"),
            "shortfall legs must be readable: {rendered}"
        );
    }

    /// One ATA rent is added only when the route creates the
    /// destination WSOL ATA (Raydium/PumpSwap). A wallet that clears
    /// the curve floor still refuses those routes, and one at exactly
    /// the ATA-carrying floor passes both spellings.
    #[tokio::test]
    async fn sell_solvency_scopes_ata_rent_to_routes_that_create_it() {
        let guard = test_wallet();
        // 900_000: above fee+floor (895_880), below the ATA-carrying
        // floor (895_880 + 2_039_280 = 2_935_160).
        let rpc = mock_rpc(&[900_000]);
        guard
            .assert_sell_solvency(&rpc, 0, 0, false)
            .await
            .expect("no-ATA route: fee legs are enough");
        let err = guard
            .assert_sell_solvency(&mock_rpc(&[900_000]), 0, 0, true)
            .await
            .expect_err("ATA-creating route needs the rent leg");
        assert!(
            matches!(
                err,
                WalletGuardError::SellInsufficientFunds {
                    ata_rent_lamports: 2_039_280,
                    ..
                }
            ),
            "{err}"
        );
        // At the exact ATA-carrying requirement, both spellings pass.
        let at = mock_rpc(&[2_935_160]);
        guard
            .assert_sell_solvency(&at, 0, 0, true)
            .await
            .expect("exact ATA floor passes");
        guard
            .assert_sell_solvency(&mock_rpc(&[2_935_160]), 0, 0, false)
            .await
            .expect("no-ATA route needs less, so passes too");
    }

    /// Buys keep [`WalletGuard::assert_solvency`] exactly as
    /// before — the 0.05 SOL reserve is a *buy* funding rule. A 0.04
    /// SOL wallet that passes every sell leg is refused for a buy, and
    /// the refusal is the reserve error, not the sell variant.
    #[tokio::test]
    async fn buy_solvency_still_requires_005_reserve() {
        let guard = test_wallet();
        let err = guard
            .assert_solvency(&mock_rpc(&[40_000_000]), 0, 100_000, 0)
            .await
            .expect_err("buy must keep the reserve");
        match err {
            WalletGuardError::InsufficientFunds {
                available_lamports,
                required_lamports,
            } => {
                // trade 0 + tip 100_000 + priority 0 + fee 5_000 + one
                // ATA rent 2_039_280 + reserve 50_000_000.
                assert_eq!(available_lamports, 40_000_000);
                assert_eq!(required_lamports, 52_144_280);
            }
            other => panic!("reserve refusal expected, got {other}"),
        }
        // At the exact requirement the buy passes.
        guard
            .assert_solvency(&mock_rpc(&[52_144_280]), 0, 100_000, 0)
            .await
            .expect("exact reserve passes for buys");
    }

    #[test]
    fn missing_env_errors() {
        let _guard = env_lock().lock().expect("lock");
        let saved = env::var("SOLANA_PRIVATE_KEY").ok();
        let saved_file = env::var("SOLANA_PRIVATE_KEY_FILE").ok();
        unsafe { env::remove_var("SOLANA_PRIVATE_KEY") };
        unsafe { env::remove_var("SOLANA_PRIVATE_KEY_FILE") };
        let err = match WalletGuard::load_from_env() {
            Err(e) => e,
            Ok(_) => panic!("must fail"),
        };
        assert!(matches!(err, WalletGuardError::MissingKeyEnv));
        if let Some(v) = saved {
            unsafe { env::set_var("SOLANA_PRIVATE_KEY", v) };
        }
        if let Some(v) = saved_file {
            unsafe { env::set_var("SOLANA_PRIVATE_KEY_FILE", v) };
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
    fn loads_from_file_when_env_unset() {
        // Preferred path: the key never enters the environment
        // (`ps e`, shell history, `/proc` stay clean).
        let _guard = env_lock().lock().expect("lock");
        let saved = env::var("SOLANA_PRIVATE_KEY").ok();
        let saved_file = env::var("SOLANA_PRIVATE_KEY_FILE").ok();
        unsafe { env::remove_var("SOLANA_PRIVATE_KEY") };
        let dir = env::temp_dir().join(format!(
            "wallet-file-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("key.txt");
        let keypair = Keypair::new();
        std::fs::write(
            &path,
            format!("  {}\n", bs58::encode(keypair.to_bytes()).into_string()),
        )
        .expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("chmod 600");
        }
        unsafe { env::set_var("SOLANA_PRIVATE_KEY_FILE", &path) };
        let loaded = WalletGuard::load_from_env().expect("load");
        assert_eq!(loaded.pubkey(), keypair.pubkey());
        unsafe { env::remove_var("SOLANA_PRIVATE_KEY_FILE") };
        match saved {
            Some(v) => unsafe { env::set_var("SOLANA_PRIVATE_KEY", v) },
            None => unsafe { env::remove_var("SOLANA_PRIVATE_KEY") },
        }
        if let Some(v) = saved_file {
            unsafe { env::set_var("SOLANA_PRIVATE_KEY_FILE", v) };
        }
        let _ = std::fs::remove_dir_all(&dir);
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
        // Post-load scrub: the variable is gone afterwards.
        assert!(env::var("SOLANA_PRIVATE_KEY").is_err());
        match saved {
            Some(v) => unsafe { env::set_var("SOLANA_PRIVATE_KEY", v) },
            None => unsafe { env::remove_var("SOLANA_PRIVATE_KEY") },
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_group_or_world_readable_key_file() {
        use std::os::unix::fs::PermissionsExt;
        let _guard = env_lock().lock().expect("lock");
        let saved = env::var("SOLANA_PRIVATE_KEY").ok();
        let saved_file = env::var("SOLANA_PRIVATE_KEY_FILE").ok();
        unsafe { env::remove_var("SOLANA_PRIVATE_KEY") };
        let dir = env::temp_dir().join(format!(
            "wallet-perms-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("key.txt");
        std::fs::write(&path, bs58::encode(Keypair::new().to_bytes()).into_string())
            .expect("write");
        // Owner-only: fine.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod 600");
        assert!(check_key_file_permissions(&path).is_ok());
        // Group/world bits: refused before a byte of key material is read.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod 644");
        let err = check_key_file_permissions(&path).expect_err("refused");
        assert!(
            matches!(err, WalletGuardError::InsecureKeyFile { mode: 0o644, .. }),
            "{err}"
        );
        unsafe { env::set_var("SOLANA_PRIVATE_KEY_FILE", &path) };
        let err = match WalletGuard::load_from_env() {
            Err(e) => e,
            Ok(_) => panic!("refused"),
        };
        assert!(
            matches!(err, WalletGuardError::InsecureKeyFile { .. }),
            "{err}"
        );
        unsafe { env::remove_var("SOLANA_PRIVATE_KEY_FILE") };
        if let Some(v) = saved {
            unsafe { env::set_var("SOLANA_PRIVATE_KEY", v) };
        }
        if let Some(v) = saved_file {
            unsafe { env::set_var("SOLANA_PRIVATE_KEY_FILE", v) };
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hot_wallet_cap_refuses_only_over_cap() {
        let guard =
            WalletGuard::from_base58(&bs58::encode(Keypair::new().to_bytes()).into_string())
                .expect("load");
        // Unset (default): any balance passes.
        assert!(guard.check_hot_wallet_cap(u64::MAX).is_ok());
        let armed = guard.with_hot_wallet_cap(2_000_000_000);
        assert!(
            armed.check_hot_wallet_cap(2_000_000_000).is_ok(),
            "at the cap passes"
        );
        assert!(armed.check_hot_wallet_cap(1).is_ok());
        let err = armed
            .check_hot_wallet_cap(2_000_000_001)
            .expect_err("over cap");
        assert!(
            matches!(
                err,
                WalletGuardError::HotWalletOverCap {
                    available_lamports: 2_000_000_001,
                    cap_lamports: 2_000_000_000
                }
            ),
            "{err}"
        );
    }
}

//! Swap dispatch: solvency-guarded build, sign, simulate, and confirm.
//!
//! Every order runs [`WalletGuard::assert_solvency`] first — a broke or
//! underfunded wallet fails *before* any instruction is built — then a
//! `simulateTransaction` preflight rejects definitive failures before
//! fees burn (transport trouble proceeds with a log). Sends go
//! through standard RPC confirmation (`buy`/`sell`) or Jito bundles
//! (`buy_jito`/`sell_jito` with an in-bundle tip). Priority fees attach
//! via `with_compute_budget`.

use solana_client::{client_error::ClientError, nonblocking::rpc_client::RpcClient};
use solana_commitment_config::CommitmentConfig;
use solana_pubkey::Pubkey;
use solana_transaction::Transaction;
use std::{str::FromStr, sync::Arc, time::Duration};
use thiserror::Error;
use tracing::{info, warn};

use crate::instructions::{PumpSwapBuilder, SwapKind, user_ata_with_program};
use crate::jito::{
    JitoClient, JitoError, encode_bundle, nanos_seed, select_tip_account, tip_instruction,
};
use crate::wallet::{WalletGuard, WalletGuardError};

/// Executor failures: config, guard, build, and RPC errors.
#[derive(Error, Debug)]
pub enum ExecutorError {
    /// RPC endpoint URL is invalid.
    #[error("invalid rpc url: {0}")]
    BadUrl(String),
    /// Wallet guard rejected the order.
    #[error("wallet guard: {0}")]
    Wallet(#[from] WalletGuardError),
    /// RPC failure (blockhash, send, confirm).
    #[error("rpc error: {0}")]
    Rpc(#[from] ClientError),
    /// Jito dispatch failure.
    #[error("jito error: {0}")]
    Jito(#[from] JitoError),
    /// Jito route requested but no bundle client configured.
    #[error("jito client not configured; call with_jito first")]
    NoJito,
    /// Signature is not valid base58: {value} ({msg}).
    #[error("bad signature '{value}': {msg}")]
    BadSignature {
        /// Offending signature string.
        value: String,
        /// Parse failure message.
        msg: String,
    },
    /// Transaction has no signatures after signing (refusing to dispatch).
    #[error("unsigned transaction: refusing to dispatch")]
    UnsignedTransaction,
    /// Simulation proved the transaction would fail (refusing to burn fees).
    #[error("simulateTransaction failed: {0}")]
    SimFailed(String),
    /// Order amounts are degenerate (refusing a doomed transaction).
    #[error("bad order: {0}")]
    BadOrder(String),
    /// RPC call exceeded its deadline.
    #[error("rpc call timed out: {0}")]
    Timeout(&'static str),
}

/// Live buy parameters (base units and lamports — no floats).
#[derive(Debug, Clone)]
pub struct BuyOrder {
    /// Token mint.
    pub mint: Pubkey,
    /// Bonding-curve PDA.
    pub bonding_curve: Pubkey,
    /// Curve's associated token account.
    pub associated_bonding_curve: Pubkey,
    /// Token base units to buy.
    pub amount_tokens: u64,
    /// Slippage guard: max lamports to spend.
    pub max_sol_cost: u64,
    /// Jito tip budget in lamports, paid as an in-bundle system transfer
    /// on `buy_jito`/`sell_jito` (plain-RPC `buy`/`sell` ignore it).
    pub tip_lamports: u64,
    /// Token creator, when known: selects the extended mayhem swap shape
    /// (creator vault + volume/fee routing). `None` keeps the legacy core.
    pub creator: Option<Pubkey>,
}

/// Live sell parameters.
#[derive(Debug, Clone)]
pub struct SellOrder {
    /// Token mint.
    pub mint: Pubkey,
    /// Bonding-curve PDA.
    pub bonding_curve: Pubkey,
    /// Curve's associated token account.
    pub associated_bonding_curve: Pubkey,
    /// Token base units to sell.
    pub amount_tokens: u64,
    /// Slippage guard: min lamports to accept.
    pub min_sol_output: u64,
    /// Jito tip budget in lamports (see [`BuyOrder::tip_lamports`]).
    pub tip_lamports: u64,
    /// Token creator, when known: selects the extended mayhem swap shape.
    /// A known creator means a `create_v2` launch, whose curve takes the
    /// 26-account `sell_v2` shape (see `preflight_sell_v2`); `None` keeps
    /// the legacy core.
    pub creator: Option<Pubkey>,
}

/// Opaque v2-sell routing lifted from a same-curve reference sell:
/// quote mint [2], pfee vault [8] + quote account [9], readonly [18].
/// Per-curve stable (like buyback routing); never derived, never
/// guessed — absence fails the sell closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SellV2Reference {
    /// Reference transaction signature (base58, for logs).
    pub signature: String,
    /// Quote mint (position 2).
    pub quote_mint: Pubkey,
    /// pfee quote vault (position 8).
    pub pfee_vault: Pubkey,
    /// pfee vault's quote account (position 9).
    pub pfee_quote_account: Pubkey,
    /// Readonly reference account (position 18).
    pub unknown_18: Pubkey,
}

/// Signature-scan depth for reference resolution (bounded RPC budget).
pub const REFERENCE_SCAN_LIMIT: usize = 20;

/// Pick the first top-level `sell_v2` on OUR curve+mint from a raw
/// `getTransaction` Json envelope and lift its opaque routing. Pure;
/// `None` on any shape mismatch (wrong curve/mint, short accounts,
/// undecodable data) — callers keep scanning, never misattribute.
/// Account keys resolve static-first then address-table order
/// (writable, readonly), mirroring the canonical resolvers.
pub fn select_sell_v2_reference(
    tx: &serde_json::Value,
    curve: &Pubkey,
    mint: &Pubkey,
) -> Option<SellV2Reference> {
    let msg = tx.get("transaction")?.get("message")?;
    let mut keys: Vec<String> = msg
        .get("accountKeys")?
        .as_array()?
        .iter()
        .filter_map(|k| k.as_str().map(str::to_string))
        .collect();
    if let Some(loaded) = tx
        .get("meta")
        .and_then(|m| m.get("loadedAddresses"))
        .and_then(|l| l.as_object())
    {
        for table in ["writable", "readonly"] {
            if let Some(addrs) = loaded.get(table).and_then(|v| v.as_array()) {
                keys.extend(addrs.iter().filter_map(|k| k.as_str().map(str::to_string)));
            }
        }
    }
    let pump = crate::instructions::PUMP_PROGRAM_ID.to_string();
    let pump_idx = keys.iter().position(|k| k == &pump)? as u64;
    let ixs = msg.get("instructions")?.as_array()?;
    let sig = tx
        .get("transaction")?
        .get("signatures")?
        .as_array()?
        .first()?
        .as_str()?
        .to_string();
    for ix in ixs {
        let Some(pi) = ix.get("programIdIndex").and_then(|v| v.as_u64()) else {
            continue;
        };
        if pi != pump_idx {
            continue;
        }
        let Some(data_str) = ix.get("data").and_then(|v| v.as_str()) else {
            continue;
        };
        let Ok(raw) = bs58::decode(data_str).into_vec() else {
            continue;
        };
        if !raw.starts_with(&crate::instructions::PUMP_SELL_V2_DISCRIMINATOR) || raw.len() != 24 {
            continue;
        }
        let Some(arr) = ix.get("accounts").and_then(|v| v.as_array()) else {
            continue;
        };
        let idx: Vec<usize> = arr
            .iter()
            .filter_map(|a| a.as_u64().map(|i| i as usize))
            .collect();
        if idx.len() != 26 {
            continue;
        }
        let at = |pos: usize| -> Option<Pubkey> {
            keys.get(*idx.get(pos)?)
                .and_then(|k| Pubkey::from_str(k).ok())
        };
        // Must be OUR curve+mint: a shared-tx stranger sell must never
        // supply routing for another market.
        if at(10).as_ref() != Some(curve) || at(1).as_ref() != Some(mint) {
            continue;
        }
        return Some(SellV2Reference {
            signature: sig,
            quote_mint: at(2)?,
            pfee_vault: at(8)?,
            pfee_quote_account: at(9)?,
            unknown_18: at(18)?,
        });
    }
    None
}

/// Fill landing outcome for a dispatched signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmOutcome {
    /// Landed and succeeded on-chain.
    Confirmed,
    /// Landed but the transaction errored (no fill).
    FailedOnChain,
    /// Still unknown after the deadline.
    Timeout,
}

/// Confirmed swap receipt.
#[derive(Debug, Clone)]
pub struct SwapReceipt {
    /// Confirmed transaction signature (base58).
    pub signature: String,
}

/// Accepted Jito bundle receipt (acceptance ≠ landing: the bundle may
/// still miss its slot; track the signature separately).
#[derive(Debug, Clone)]
pub struct JitoReceipt {
    /// Block-engine bundle UUID.
    pub bundle_id: String,
    /// First transaction signature (base58) for tracking.
    pub signature: String,
}

/// Live swap executor. Cloneable; clients and wallet are shared.
#[derive(Clone)]
pub struct SwapExecutor {
    rpc: Arc<RpcClient>,
    wallet: Arc<WalletGuard>,
    jito: Option<JitoClient>,
    compute_unit_limit: u32,
    compute_unit_price: u64,
    /// Explicit fee-recipient override (`None` = resolve from Global at
    /// preflight, canonical constant as fallback; the program rotates
    /// recipients so hardcoding fails closed into fee misdirection).
    fee_recipient_override: Option<Pubkey>,
    /// Strict preflight: transport/timeout simulation failures reject
    /// the dispatch instead of proceeding with a warning. Default off
    /// (today's behavior); enable for live funds via `SIM_STRICT=true`.
    strict_sim: bool,
}

/// Global account layout: 8-byte Anchor discriminator, then
/// `initialized`, `authority`, `fee_recipient` — the recipient slot is
/// bytes 41..73. (An older client-SDK doc quoted 33..65, i.e. the same
/// struct WITHOUT the discriminator prefix; reading there yields a
/// garbage span that the program rejects with `NotAuthorized`. Verified
/// live: 41..73 currently holds a rotated recipient seen the same day
/// in real sells.)
pub const GLOBAL_FEE_RECIPIENT_OFFSET: usize = 41;
/// End offset (exclusive) of the fee-recipient slot in Global data.
pub const GLOBAL_FEE_RECIPIENT_END: usize = 73;

/// Read the current fee recipient from the Global account. `None` on any
/// failure (transport, short data): callers fall back to the canonical
/// constant. Deadline-bound.
pub async fn fetch_global_fee_recipient(rpc: &RpcClient) -> Option<Pubkey> {
    let account = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        rpc.get_account(&crate::instructions::GLOBAL_ACCOUNT),
    )
    .await
    {
        Ok(Ok(account)) => account,
        _ => return None,
    };
    let bytes: [u8; 32] = account
        .data
        .get(GLOBAL_FEE_RECIPIENT_OFFSET..GLOBAL_FEE_RECIPIENT_END)?
        .try_into()
        .ok()?;
    Some(Pubkey::new_from_array(bytes))
}

impl SwapExecutor {
    /// Build against an HTTP RPC URL with explicit commitment.
    pub fn new(
        http_url: &str,
        commitment: CommitmentConfig,
        wallet: WalletGuard,
    ) -> Result<Self, ExecutorError> {
        match url::Url::parse(http_url) {
            Ok(u) if u.scheme() == "http" || u.scheme() == "https" => {}
            _ => return Err(ExecutorError::BadUrl(http_url.to_string())),
        }
        Ok(Self {
            rpc: Arc::new(RpcClient::new_with_commitment(
                http_url.to_string(),
                commitment,
            )),
            wallet: Arc::new(wallet),
            jito: None,
            compute_unit_limit: 0,
            compute_unit_price: 0,
            fee_recipient_override: None,
            strict_sim: false,
        })
    }

    /// Attach a Jito bundle client for protected dispatch.
    pub fn with_jito(mut self, jito: JitoClient) -> Self {
        self.jito = Some(jito);
        self
    }

    /// Override the fee-recipient account used in swap instructions.
    /// Without this, the recipient resolves from the Global account at
    /// preflight time (canonical constant as fallback). Set explicitly
    /// only to pin a recipient (e.g. tests, emergencies).
    pub fn with_fee_recipient(mut self, fee_recipient: Pubkey) -> Self {
        self.fee_recipient_override = Some(fee_recipient);
        self
    }

    /// Strict preflight: simulation transport/timeout failures reject the
    /// dispatch (`SimFailed`) instead of proceeding unproven. Off by
    /// default (an unknown sim result preserves today's behavior rather
    /// than blocking valid trades); turn on for live funds.
    pub fn with_strict_sim(mut self, strict: bool) -> Self {
        self.strict_sim = strict;
        self
    }

    /// Resolve the fee recipient: explicit override, else the Global
    /// account's current recipient, else the canonical constant.
    /// Deadline-bound; never fails (worst case is the constant).
    async fn fee_recipient(&self) -> Pubkey {
        if let Some(pinned) = self.fee_recipient_override {
            return pinned;
        }
        if let Some(live) = fetch_global_fee_recipient(&self.rpc).await {
            return live;
        }
        crate::instructions::FEE_RECIPIENT
    }

    /// Resolve the token program owning `mint` (classic SPL vs
    /// Token-2022): v2/mayhem mints live under Token-2022 and a swap
    /// naming the wrong program fails on-chain. Fail-closed (`None`) when
    /// the mint account is unreadable — building with a guessed program
    /// burns fees for certain.
    async fn token_program(&self, mint: &Pubkey) -> Option<Pubkey> {
        let account = match tokio::time::timeout(
            Duration::from_secs(10),
            self.rpc.get_account(mint),
        )
        .await
        {
            Ok(Ok(account)) => account,
            _ => return None,
        };
        Some(account.owner)
    }

    /// Attach a compute budget: `SetComputeUnitLimit` + `SetComputeUnitPrice`
    /// are prepended to every built transaction (limit first, then price,
    /// per runtime convention). Zero values disable that instruction, so
    /// the default executor builds byte-identical transactions to before.
    pub fn with_compute_budget(mut self, unit_limit: u32, unit_price_micro_lamports: u64) -> Self {
        self.compute_unit_limit = unit_limit;
        self.compute_unit_price = unit_price_micro_lamports;
        self
    }

    /// Compute-budget prefix for the next transaction (empty by default).
    fn budget_prefix(&self) -> Vec<solana_instruction::Instruction> {
        crate::instructions::compute_budget_ixs(self.compute_unit_limit, self.compute_unit_price)
    }
}

impl SwapExecutor {
    /// Trading address.
    pub fn pubkey(&self) -> Pubkey {
        self.wallet.pubkey()
    }

    /// Poll a dispatched signature until it lands or `timeout` elapses.
    /// `Some(Ok(()))` (at the client commitment) is a fill;
    /// `Some(Err(_))` landed-but-errored; `None` is still unknown.
    /// Never panics; transport errors propagate.
    pub async fn confirm_signature(
        &self,
        signature_str: &str,
        timeout: Duration,
    ) -> Result<ConfirmOutcome, ExecutorError> {
        use std::time::Instant;
        let signature = solana_signature::Signature::from_str(signature_str).map_err(|e| {
            ExecutorError::BadSignature {
                value: signature_str.to_string(),
                msg: e.to_string(),
            }
        })?;
        let start = Instant::now();
        loop {
            // Per-poll deadline: one hung RPC must stall only this round,
            // never the whole confirmation loop (the outer `timeout` is the
            // total budget, this is the liveness guard per attempt).
            let status = match tokio::time::timeout(
                Duration::from_secs(15),
                self.rpc.get_signature_status(&signature),
            )
            .await
            {
                Ok(Ok(status)) => status,
                // Transport error: fail fast (caller decides retry/fallback).
                Ok(Err(e)) => return Err(e.into()),
                // Poll hung: treat as still-unknown and keep waiting only
                // while the total budget allows.
                Err(_) => None,
            };
            match status {
                Some(Ok(())) => return Ok(ConfirmOutcome::Confirmed),
                Some(Err(_)) => return Ok(ConfirmOutcome::FailedOnChain),
                None => {}
            }
            if start.elapsed() >= timeout {
                return Ok(ConfirmOutcome::Timeout);
            }
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
    }

    /// Buy `amount_tokens` for at most `max_sol_cost` lamports.
    pub async fn buy(&self, order: &BuyOrder) -> Result<SwapReceipt, ExecutorError> {
        let ixs = self.preflight_buy(order).await?;
        self.send(&ixs, "buy").await
    }

    /// Sell `amount_tokens` for at least `min_sol_output` lamports.
    pub async fn sell(&self, order: &SellOrder) -> Result<SwapReceipt, ExecutorError> {
        let ixs = self.preflight_sell(order).await?;
        self.send(&ixs, "sell").await
    }

    /// Buy via Jito bundle: swap ix plus the order's tip in one atomic
    /// transaction. Requires [`SwapExecutor::with_jito`].
    pub async fn buy_jito(&self, order: &BuyOrder) -> Result<JitoReceipt, ExecutorError> {
        let mut ixs = self.preflight_buy(order).await?;
        self.push_tip(&mut ixs, order.tip_lamports).await;
        self.send_bundle(&ixs, "buy-jito").await
    }

    /// Sell via Jito bundle (see [`SwapExecutor::buy_jito`]).
    pub async fn sell_jito(&self, order: &SellOrder) -> Result<JitoReceipt, ExecutorError> {
        let mut ixs = self.preflight_sell(order).await?;
        self.push_tip(&mut ixs, order.tip_lamports).await;
        self.send_bundle(&ixs, "sell-jito").await
    }

    /// Shared buy path: solvency preflight plus swap instruction.
    async fn preflight_buy(
        &self,
        order: &BuyOrder,
    ) -> Result<Vec<solana_instruction::Instruction>, ExecutorError> {
        // Degenerate orders can never fill: reject before spending fees.
        // (`LiveTrader::plan_buy` already filters these; this guards
        // direct `SwapExecutor` callers.)
        if order.amount_tokens == 0 {
            return Err(ExecutorError::BadOrder("buy amount_tokens is 0".to_string()));
        }
        if order.max_sol_cost == 0 {
            return Err(ExecutorError::BadOrder("buy max_sol_cost is 0".to_string()));
        }
        let balance = self
            .wallet
            .assert_solvency(&self.rpc, order.max_sol_cost, order.tip_lamports)
            .await?;
        info!(mint = %order.mint, balance, max_cost = order.max_sol_cost, "buy pre-flight ok");
        let payer = self.wallet.pubkey();
        let Some(token_program) = self.token_program(&order.mint).await else {
            return Err(ExecutorError::BadOrder("mint account unreadable; token program unknown".to_string()));
        };
        // Program-bound ATA: Token-2022 mints derive a DIFFERENT address
        // than the classic helper (verified live).
        let ata = user_ata_with_program(&payer, &order.mint, &token_program);
        let mut ixs = self
            .maybe_create_ata_ix(&payer, &order.mint, &ata, &token_program)
            .await;
        let fee_recipient = self.fee_recipient().await;
        ixs.push(PumpSwapBuilder::build_swap_ix(
            SwapKind::Buy { amount: order.amount_tokens, max_sol_cost: order.max_sol_cost, track_volume: true },
            &crate::instructions::SwapAccounts {
                payer: &payer,
                mint: &order.mint,
                bonding_curve: &order.bonding_curve,
                associated_bonding_curve: &order.associated_bonding_curve,
                user_ata: &ata,
                fee_recipient: &fee_recipient,
                token_program: &token_program,
                creator: order.creator.as_ref(),
            },
        ));
        Ok(ixs)
    }

    /// Shared sell path: solvency preflight plus swap instruction.
    /// Sells spend no SOL beyond fees; still enforce reserve + tip.
    async fn preflight_sell(
        &self,
        order: &SellOrder,
    ) -> Result<Vec<solana_instruction::Instruction>, ExecutorError> {
        if order.amount_tokens == 0 {
            return Err(ExecutorError::BadOrder("sell amount_tokens is 0".to_string()));
        }
        let balance = self
            .wallet
            .assert_solvency(&self.rpc, 0, order.tip_lamports)
            .await?;
        info!(mint = %order.mint, balance, amount = order.amount_tokens, "sell pre-flight ok");
        let payer = self.wallet.pubkey();
        let Some(token_program) = self.token_program(&order.mint).await else {
            return Err(ExecutorError::BadOrder("mint account unreadable; token program unknown".to_string()));
        };
        // Program-bound ATA (see buy path): the balance check below must
        // read the account the swap will actually spend from.
        let ata = user_ata_with_program(&payer, &order.mint, &token_program);
        // The wallet must actually hold the tokens: an underfunded ATA
        // fails on-chain after burning fees, so check the balance first.
        match tokio::time::timeout(
            Duration::from_secs(10),
            self.rpc.get_token_account_balance(&ata),
        )
        .await
        {
            Ok(Ok(balance)) => {
                let held: u64 = balance.amount.parse().unwrap_or(0);
                if held < order.amount_tokens {
                    return Err(ExecutorError::BadOrder(format!(
                        "sell amount {} exceeds ATA balance {held}",
                        order.amount_tokens
                    )));
                }
            }
            Ok(Err(_)) => {
                // ATA missing/unreadable: the sell cannot fill; fail here
                // instead of after fees.
                return Err(ExecutorError::BadOrder(
                    "sell ATA missing or unreadable".to_string(),
                ));
            }
            Err(_) => return Err(ExecutorError::Timeout("sell balance read")),
        }
        let fee_recipient = self.fee_recipient().await;
        // v2 curves (creator is only known for `create_v2` launches)
        // reject legacy sells outright — route them at the verified
        // 26-account `sell_v2` shape instead of burning fees. v1 keeps
        // the legacy core below.
        if let Some(creator) = order.creator.as_ref() {
            return self
                .preflight_sell_v2(order, &payer, &token_program, &ata, &fee_recipient, creator)
                .await;
        }
        Ok(vec![PumpSwapBuilder::build_swap_ix(
            SwapKind::Sell { amount: order.amount_tokens, min_sol_output: order.min_sol_output },
            &crate::instructions::SwapAccounts {
                payer: &payer,
                mint: &order.mint,
                bonding_curve: &order.bonding_curve,
                associated_bonding_curve: &order.associated_bonding_curve,
                user_ata: &ata,
                fee_recipient: &fee_recipient,
                token_program: &token_program,
                // Always None here: creator-carrying orders return via
                // the v2 path above (legacy sells die on v2 curves).
                creator: None,
            },
        )])
    }

    /// v2 sell path: the verified 26-account `sell_v2` shape. Opaque
    /// routing (quote mint, pfee vault + account, [18]) resolves from a
    /// recent same-curve reference sell — per-curve stable like buyback
    /// routing — and fails closed when none scans. The SOL floor is the
    /// order's own `min_sol_output` (two-sample confirmed semantics),
    /// never a copied value.
    ///
    /// Young-curve edge: a curve with zero prior sells has no reference
    /// yet — the sell fails closed here and the caller's retry path
    /// (cooldown + budget, then manual exit) covers the wait, exactly
    /// like any other unconfirmed live sell.
    async fn preflight_sell_v2(
        &self,
        order: &SellOrder,
        payer: &Pubkey,
        token_program: &Pubkey,
        base_ata: &Pubkey,
        fee_recipient: &Pubkey,
        creator: &Pubkey,
    ) -> Result<Vec<solana_instruction::Instruction>, ExecutorError> {
        let reference = self
            .resolve_sell_v2_reference(&order.bonding_curve, &order.mint)
            .await?;
        let Some(quote_program) = self.token_program(&reference.quote_mint).await else {
            return Err(ExecutorError::BadOrder(
                "quote mint account unreadable; token program unknown".to_string(),
            ));
        };
        warn!(mint = %order.mint, curve = %order.bonding_curve, ref_sig = %reference.signature,
            quote = %reference.quote_mint, floor = order.min_sol_output,
            "v2 sell routed via same-curve reference (opaque routing copied, guard computed)");
        let quote_ata = crate::instructions::user_ata_with_program(
            payer,
            &reference.quote_mint,
            &quote_program,
        );
        let mut ixs = self
            .maybe_create_ata_ix(payer, &order.mint, base_ata, token_program)
            .await;
        ixs.extend(
            self.maybe_create_ata_ix(payer, &reference.quote_mint, &quote_ata, &quote_program)
                .await,
        );
        ixs.push(PumpSwapBuilder::build_sell_v2_ix(
            order.amount_tokens,
            order.min_sol_output,
            &crate::instructions::SellV2Accounts {
                payer,
                mint: &order.mint,
                quote_mint: &reference.quote_mint,
                bonding_curve: &order.bonding_curve,
                associated_bonding_curve: &order.associated_bonding_curve,
                fee_recipient,
                base_token_program: token_program,
                quote_token_program: &quote_program,
                creator,
                pfee_vault: &reference.pfee_vault,
                pfee_quote_account: &reference.pfee_quote_account,
                unknown_18: &reference.unknown_18,
            },
        ));
        Ok(ixs)
    }

    /// Scan recent curve transactions for a top-level `sell_v2` on OUR
    /// curve+mint and lift its opaque routing (quote mint/fee, pfee
    /// vault + account, [18]). Bounded (signature page + per-tx
    /// deadline); fail-closed `BadOrder` when nothing resolves — a v2
    /// sell without a reference is unroutable, never guessed.
    async fn resolve_sell_v2_reference(
        &self,
        curve: &Pubkey,
        mint: &Pubkey,
    ) -> Result<SellV2Reference, ExecutorError> {
        use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
        let sigs = match tokio::time::timeout(
            Duration::from_secs(10),
            self.rpc.get_signatures_for_address_with_config(
                curve,
                GetConfirmedSignaturesForAddress2Config {
                    before: None,
                    until: None,
                    limit: Some(REFERENCE_SCAN_LIMIT),
                    commitment: None,
                },
            ),
        )
        .await
        {
            Ok(Ok(sigs)) => sigs,
            Ok(Err(e)) => return Err(ExecutorError::Rpc(e)),
            Err(_) => return Err(ExecutorError::Timeout("reference signature scan")),
        };
        for status in sigs {
            if status.err.is_some() {
                continue;
            }
            let tx = match self.fetch_tx_json(&status.signature).await {
                Some(tx) => tx,
                None => continue,
            };
            if let Some(reference) = select_sell_v2_reference(&tx, curve, mint) {
                return Ok(reference);
            }
        }
        Err(ExecutorError::BadOrder(
            "no reference sell_v2 on curve; v2 sell unroutable".to_string(),
        ))
    }

    /// One raw `getTransaction` (Json + v1 envelopes, like every other
    /// reader in the tree). `None` on any transport/timeout/shape
    /// failure — callers skip loudly, never misparse.
    async fn fetch_tx_json(&self, signature: &str) -> Option<serde_json::Value> {
        use solana_client::rpc_request::RpcRequest;
        let params = serde_json::json!([
            signature,
            {"encoding": "json", "maxSupportedTransactionVersion": 1},
        ]);
        match tokio::time::timeout(
            Duration::from_secs(5),
            self.rpc
                .send::<serde_json::Value>(RpcRequest::GetTransaction, params),
        )
        .await
        {
            Ok(Ok(tx)) => Some(tx),
            _ => None,
        }
    }

    /// Create-ATA prefix when the user's token account for `mint` is not
    /// known to exist (fresh wallet or first touch of the mint). The
    /// idempotent variant makes the prefix safe on all paths: an existing
    /// ATA succeeds silently, and a race between the existence check and
    /// execution cannot fail the bundle.
    async fn maybe_create_ata_ix(
        &self,
        payer: &Pubkey,
        mint: &Pubkey,
        ata: &Pubkey,
        token_program: &Pubkey,
    ) -> Vec<solana_instruction::Instruction> {
        match tokio::time::timeout(Duration::from_secs(10), self.rpc.get_account(ata)).await {
            Ok(Ok(_)) => Vec::new(),
            _ => {
                use crate::instructions::build_create_ata_ix;
                vec![build_create_ata_ix(payer, payer, mint, ata, token_program)]
            }
        }
    }

    /// Append the tip transfer when the budget is non-zero. Uses the live
    /// tip set when a bundle client is attached (hourly refresh, silent
    /// fallback to the static rotation); plain-RPC sends use the static
    /// rotation directly (no engine to ask).
    async fn push_tip(&self, ixs: &mut Vec<solana_instruction::Instruction>, tip_lamports: u64) {
        if tip_lamports == 0 {
            return;
        }
        let payer = self.wallet.pubkey();
        let tip = match &self.jito {
            Some(jito) => jito.tip_account(nanos_seed()).await,
            None => select_tip_account(nanos_seed()),
        };
        ixs.push(tip_instruction(&payer, &tip, tip_lamports));
    }

    /// Sign with the guarded keypair and confirm. Single-flight per call.
    /// Deadline-bound on top of the client's own confirmation loop.
    async fn send(
        &self,
        ixs: &[solana_instruction::Instruction],
        label: &str,
    ) -> Result<SwapReceipt, ExecutorError> {
        let tx = self.build_tx(ixs).await?;
        self.simulate_tx(&tx, label).await?;
        match tokio::time::timeout(
            std::time::Duration::from_secs(90),
            self.rpc.send_and_confirm_transaction(&tx),
        )
        .await
        {
            Ok(Ok(signature)) => {
                info!(%label, %signature, "swap confirmed");
                Ok(SwapReceipt {
                    signature: signature.to_string(),
                })
            }
            Ok(Err(e)) => {
                warn!(%label, error = %e, "swap failed");
                Err(ExecutorError::Rpc(e))
            }
            Err(_) => {
                warn!(%label, "swap confirmation timed out");
                Err(ExecutorError::Timeout("send_and_confirm"))
            }
        }
    }

    /// Sign and dispatch as a single-transaction Jito bundle.
    async fn send_bundle(
        &self,
        ixs: &[solana_instruction::Instruction],
        label: &str,
    ) -> Result<JitoReceipt, ExecutorError> {
        let jito = self.jito.clone().ok_or(ExecutorError::NoJito)?;
        let tx = self.build_tx(ixs).await?;
        self.simulate_tx(&tx, label).await?;
        let signature = tx
            .signatures
            .first()
            .map(ToString::to_string)
            .filter(|s| !s.is_empty())
            .ok_or(ExecutorError::UnsignedTransaction)?;
        let bundle = encode_bundle(std::slice::from_ref(&tx))?;
        match jito.send_bundle(bundle).await {
            Ok(bundle_id) if !bundle_id.is_empty() => {
                info!(%label, %bundle_id, %signature, "bundle accepted");
                Ok(JitoReceipt {
                    bundle_id,
                    signature,
                })
            }
            Ok(_) => {
                warn!(%label, "bundle engine returned empty id");
                Err(ExecutorError::Jito(JitoError::Rpc(
                    "empty bundle id".to_string(),
                )))
            }
            Err(e) => {
                warn!(%label, error = %e, "bundle rejected");
                Err(ExecutorError::Jito(e))
            }
        }
    }

    /// Fetch a fresh blockhash and sign every instruction with the wallet.
    /// Deadline-bound: a hung blockhash stalls the whole dispatch.
    async fn build_tx(
        &self,
        ixs: &[solana_instruction::Instruction],
    ) -> Result<Transaction, ExecutorError> {
        let blockhash = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            self.rpc.get_latest_blockhash(),
        )
        .await
        .map_err(|_| ExecutorError::Timeout("get_latest_blockhash"))??;
        // Compute-budget prefix first (limit, price), then the swap/ATA/tip
        // instructions; empty by default (byte-identical to before).
        let mut all = self.budget_prefix();
        all.extend_from_slice(ixs);
        Ok(Transaction::new_signed_with_payer(
            &all,
            Some(&self.wallet.pubkey()),
            &[self.wallet.keypair()],
            blockhash,
        ))
    }

    /// `simulateTransaction` preflight on the signed transaction.
    /// A definitive simulation failure rejects the dispatch *before* any
    /// fee is spent (layout bugs surface here, not on-chain). Transport
    /// failures proceed with a warning — an unknown sim result preserves
    /// today's behavior rather than blocking valid trades — unless
    /// strict preflight is on ([`SwapExecutor::with_strict_sim`]), in
    /// which case they reject too (see [`sim_transport_outcome`]).
    async fn simulate_tx(&self, tx: &Transaction, label: &str) -> Result<(), ExecutorError> {
        use solana_client::rpc_config::RpcSimulateTransactionConfig;
        let config = RpcSimulateTransactionConfig {
            sig_verify: false,
            ..RpcSimulateTransactionConfig::default()
        };
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            self.rpc.simulate_transaction_with_config(tx, config),
        )
        .await;
        match outcome {
            Ok(Ok(response)) if response.value.err.is_some() => {
                let err = format!("{:?}", response.value.err);
                warn!(%label, %err, "simulation failed; refusing to dispatch");
                Err(ExecutorError::SimFailed(err))
            }
            Ok(Ok(_)) => {
                info!(%label, "simulation ok");
                Ok(())
            }
            Ok(Err(e)) => {
                warn!(%label, error = %e, "simulation RPC failed; proceeding without preflight");
                sim_transport_outcome(self.strict_sim, "RPC failure")
            }
            Err(_) => {
                warn!(%label, "simulation timed out; proceeding without preflight");
                sim_transport_outcome(self.strict_sim, "timeout")
            }
        }
    }
}

/// Decide an unproven preflight (simulation transport/timeout): fail
/// closed under strict mode, warn-and-proceed otherwise. Pure so the
/// policy is unit-tested without network.
fn sim_transport_outcome(strict: bool, cause: &str) -> Result<(), ExecutorError> {
    if strict {
        Err(ExecutorError::SimFailed(format!(
            "preflight unavailable ({cause}); strict mode refuses to dispatch unproven"
        )))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wallet::WalletGuard;
    use solana_keypair::Keypair;

    fn executor() -> SwapExecutor {
        let keypair = Keypair::new();
        let wallet = WalletGuard::from_base58(&bs58::encode(keypair.to_bytes()).into_string())
            .expect("wallet");
        SwapExecutor::new(
            "https://api.mainnet-beta.solana.com",
            CommitmentConfig::confirmed(),
            wallet,
        )
        .expect("executor")
    }

    #[tokio::test]
    async fn confirm_rejects_garbage_signature_without_network() {
        let err = executor()
            .confirm_signature("not-a-signature", Duration::from_secs(1))
            .await
            .expect_err("must fail");
        assert!(matches!(err, ExecutorError::BadSignature { .. }));
    }

    #[test]
    fn strict_preflight_rejects_unproven_dispatch() {
        // Default (off): unknown sim result preserves today's behavior.
        assert!(sim_transport_outcome(false, "timeout").is_ok());
        // Strict: transport/timeout failures refuse to dispatch unproven.
        for cause in ["timeout", "RPC failure"] {
            match sim_transport_outcome(true, cause) {
                Err(ExecutorError::SimFailed(msg)) => assert!(msg.contains(cause), "{msg}"),
                other => panic!("expected SimFailed, got {other:?}"),
            }
        }
        // Builder plumbing enables the policy.
        assert!(!executor().strict_sim);
        assert!(executor().with_strict_sim(true).strict_sim);
    }

    #[test]
    fn select_sell_v2_reference_lifts_opaque_routing() {
        // Frozen sample: opaque routing ([2] quote, [8]/[9] pfee, [18])
        // extracts against OUR curve+mint; strangers and shapes mismatch.
        let raw: serde_json::Value = serde_json::from_str(include_str!(
            "../../listener/tests/fixtures/sell_v2_mainnet.json"
        ))
        .expect("fixture");
        let tx = &raw["result"];
        let curve =
            Pubkey::from_str("8rRGZTqPSSwbSsGPagUrYzJf1dHZ33QdxQkWEzpEnsKx").expect("curve");
        let mint = Pubkey::from_str("GmixVU6E1xeQWjEM8LAYsiFt9dJj27Ji4oDGKkmEpump").expect("mint");
        let reference = select_sell_v2_reference(tx, &curve, &mint).expect("reference");
        assert!(!reference.signature.is_empty());
        assert_eq!(
            reference.quote_mint.to_string(),
            "RACEyWiM2ztEZcJx2AHXU2eWjhxU57x3vXn92b39dLD"
        );
        assert_eq!(
            reference.pfee_vault.to_string(),
            "5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD"
        );
        assert_eq!(
            reference.pfee_quote_account.to_string(),
            "6vquTehtsUfPNbYKQ6McujXLnye6dsvkhTnDtoN2rNbA"
        );
        assert_eq!(
            reference.unknown_18.to_string(),
            "CaJUjUeQ63qAgzV6kvW64phwkGfA5V6hRyma8A2C7Ab7"
        );
        // Wrong curve or mint: never misattribute a stranger sell.
        let stranger = Pubkey::from_str("11111111111111111111111111111111").expect("key");
        assert!(select_sell_v2_reference(tx, &stranger, &mint).is_none());
        assert!(select_sell_v2_reference(tx, &curve, &stranger).is_none());
        // Garbage JSON: None, never a panic.
        assert!(select_sell_v2_reference(&serde_json::Value::Null, &curve, &mint).is_none());
        assert!(
            select_sell_v2_reference(&serde_json::json!({"transaction": {}}), &curve, &mint)
                .is_none()
        );
    }
}

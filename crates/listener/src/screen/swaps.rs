use super::*;

/// Account index of the trader (`user`) in buy/sell instructions.
pub const IDX_SWAP_USER: usize = 6;
/// Account index of the trader in v2 (`buy_v2`/`sell_v2`) instructions:
/// the fee payer (first signer) at position 13. Verified against real
/// mainnet samples — a `buy_v2` (27 accounts; position 13 resolved to the
/// fee-payer key) and a `sell_v2` (26 accounts; position 13 resolved to
/// the fee payer whose token balance decreased by exactly the payload
/// amount; see the frozen `sell_v2_mainnet` fixture).
pub const IDX_SWAP_V2_USER: usize = 13;
/// Previous name of [`IDX_SWAP_V2_USER`] (kept: the position covers both
/// v2 sides, not buys only).
pub const IDX_BUY_V2_USER: usize = IDX_SWAP_V2_USER;
/// Account index of the trader in v3 (`buy_v3`/`sell_v3`/
/// `buy_exact_quote_in_v3`) instructions: position 8 (row 9 `user` of
/// the official 17-account TRADE_V3.md table).
pub const IDX_SWAP_V3_USER: usize = 8;
/// Account index of the creator (`user`) in create instructions.
pub const IDX_CREATE_USER: usize = 7;

/// Swap direction on the bonding curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapSide {
    /// Tokens bought with SOL.
    Buy,
    /// Tokens sold for SOL.
    Sell,
    /// Pump.fun instruction with an unmapped discriminator. Still a live
    /// fail-closed path for future discriminators: side and trader layout
    /// are unproven (the wallet is the ix's first listed account as a
    /// best-effort attribution), the amount is always 0, and quorum never
    /// counts these — they exist only for the dev fail-closed rule in
    /// [`EntryGate::observe`].
    Unknown,
}

/// One classified Pump.fun swap inside a transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapRecord {
    /// Transaction signature (base58).
    pub signature: String,
    /// Slot the transaction landed in (creation-slot rule input).
    pub slot: u64,
    /// Trader (`user` account; versioned index: v1=6, v2=13, v3=8 —
    /// see `IDX_SWAP_USER` / `IDX_SWAP_V2_USER` / `IDX_SWAP_V3_USER`).
    pub wallet: Pubkey,
    /// Direction by discriminator.
    pub side: SwapSide,
    /// Token base units moved (Borsh `amount` arg; 0 when undecodable).
    pub amount_base_units: u64,
}

impl SwapRecord {
    /// Build from strings (parses the wallet; handy for tests/plumbing).
    pub fn new(
        signature: &str,
        wallet: &str,
        side: SwapSide,
        slot: u64,
        amount_base_units: u64,
    ) -> Result<Self, ScreenError> {
        Ok(Self {
            signature: signature.to_string(),
            slot,
            wallet: parse_address(wallet)?,
            side,
            amount_base_units,
        })
    }
}

/// Classify every Pump.fun buy/sell instruction in a fetched transaction.
/// Covers legacy `buy`/`sell` plus v2 (`buy_v2`/`sell_v2`: different
pub fn extract_pump_swaps(
    fetched: &EncodedConfirmedTransactionWithStatusMeta,
    sig: &str,
) -> Vec<SwapRecord> {
    let mut out = Vec::new();
    let tx_with_meta = &fetched.transaction;
    let EncodedTransaction::Json(ui_tx) = &tx_with_meta.transaction else {
        return out;
    };
    let UiMessage::Raw(raw) = &ui_tx.message else {
        return out;
    };
    let Some(keys) = resolve_keys(raw, &tx_with_meta.meta) else {
        return out;
    };

    let mut classify = |program_id_index: u8, accounts: &[u8], data_b58: &str| {
        let program = keys
            .get(usize::from(program_id_index))
            .copied()
            .unwrap_or_default();
        if program != PUMP_PROGRAM_ID {
            return;
        }
        let Some(raw) = decode_ix_data(data_b58) else {
            return;
        };
        // (side, trader account index, amount-u64 offset): each shape
        // shares the side but not the layout. v3 rows come from the
        // official TRADE_V3.md 17-account table (user at index 8).
        let (side, user_idx, amount_off) = if raw.starts_with(&PUMP_BUY_DISCRIMINATOR) {
            (SwapSide::Buy, IDX_SWAP_USER, 8)
        } else if raw.starts_with(&PUMP_SELL_DISCRIMINATOR) {
            (SwapSide::Sell, IDX_SWAP_USER, 8)
        } else if raw.starts_with(&PUMP_BUY_V2_DISCRIMINATOR) {
            (SwapSide::Buy, IDX_SWAP_V2_USER, 8)
        } else if raw.starts_with(&PUMP_SELL_V2_DISCRIMINATOR) {
            (SwapSide::Sell, IDX_SWAP_V2_USER, 8)
        } else if raw.starts_with(&PUMP_BUY_V3_DISCRIMINATOR) {
            (SwapSide::Buy, IDX_SWAP_V3_USER, 8)
        } else if raw.starts_with(&PUMP_SELL_V3_DISCRIMINATOR) {
            (SwapSide::Sell, IDX_SWAP_V3_USER, 8)
        } else if raw.starts_with(&PUMP_BUY_EXACT_QUOTE_IN_V3_DISCRIMINATOR) {
            // Exact-spend buys carry (spend, min_out): attribute the
            // conservative token-side bound (`min_tokens_out`, second
            // u64) so monopoly math never overcounts a bundler.
            (SwapSide::Buy, IDX_SWAP_V3_USER, 16)
        } else {
            // Unmapped Pump.fun instruction: the trader layout is
            // unproven, so never attribute a side or an amount.
            // `create`/`migrate` are benign lifecycle ixs and stay
            // ignored; anything else is recorded against the first
            // listed account so the gate can fail closed when it names
            // the dev wallet.
            if raw.starts_with(&PUMP_CREATE_DISCRIMINATOR)
                || raw.starts_with(&PUMP_CREATE_V2_DISCRIMINATOR)
                || raw.starts_with(&PUMP_MIGRATE_DISCRIMINATOR)
            {
                return;
            }
            let Some(wallet) = accounts
                .first()
                .and_then(|idx| keys.get(usize::from(*idx)).copied())
            else {
                return;
            };
            out.push(SwapRecord {
                signature: sig.to_string(),
                slot: fetched.slot,
                wallet,
                side: SwapSide::Unknown,
                amount_base_units: 0,
            });
            return;
        };
        let Some(account) = accounts.get(user_idx) else {
            return;
        };
        let Some(wallet) = keys.get(usize::from(*account)).copied() else {
            return;
        };
        // Amount is the first u64 for every variant except exact-quote
        // v3 (second u64, `min_tokens_out`). Chain-proven for both v2
        // sides from frozen samples (trader = fee payer at index 13;
        // token-balance delta equals bytes 8..16 to the unit — two
        // independent `buy_v2` txs plus one `sell_v2`) and per the skill
        // for legacy `buy`/`sell`. v3 first-u64 semantics come from the
        // official TRADE_V3.md argument tables; v3 frozen samples are
        // still open work (no replay test pins them yet).
        let amount_base_units = raw
            .get(amount_off..amount_off + 8)
            .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
            .map(u64::from_le_bytes)
            .unwrap_or(0);
        out.push(SwapRecord {
            signature: sig.to_string(),
            slot: fetched.slot,
            wallet,
            side,
            amount_base_units,
        });
    };

    for_each_compiled_ix(raw, &tx_with_meta.meta, &mut classify);
    out
}

/// Count Pump.fun `create` instructions authored by `dev` in a transaction.
/// Powers the prior-launch check. Never panics.
pub fn count_dev_creates(
    fetched: &EncodedConfirmedTransactionWithStatusMeta,
    dev: &Pubkey,
) -> usize {
    let tx_with_meta = &fetched.transaction;
    let EncodedTransaction::Json(ui_tx) = &tx_with_meta.transaction else {
        return 0;
    };
    let UiMessage::Raw(raw) = &ui_tx.message else {
        return 0;
    };
    let Some(keys) = resolve_keys(raw, &tx_with_meta.meta) else {
        return 0;
    };

    let mut count = 0;
    let mut check = |program_id_index: u8, accounts: &[u8], data_b58: &str| {
        if keys
            .get(usize::from(program_id_index))
            .copied()
            .unwrap_or_default()
            != PUMP_PROGRAM_ID
        {
            return;
        }
        let Some(raw) = decode_ix_data(data_b58) else {
            return;
        };
        // Author position differs by version: v1 `user` sits at index 7,
        // v2 `user` at index 5 (14-account IDL layout). A single index
        // here silently undercounts v2 serial launchers into a pass.
        let author_idx = if raw.starts_with(&PUMP_CREATE_V2_DISCRIMINATOR) {
            IDX_CREATE_V2_USER
        } else if raw.starts_with(&PUMP_CREATE_DISCRIMINATOR) {
            IDX_CREATE_USER
        } else {
            return;
        };
        let author = accounts
            .get(author_idx)
            .and_then(|idx| keys.get(usize::from(*idx)).copied());
        if author == Some(*dev) {
            count += 1;
        }
    };
    for_each_compiled_ix(raw, &tx_with_meta.meta, &mut check);
    count
}

/// Parse a base58 address, mapping failure to [`ScreenError`].
pub fn parse_address(s: &str) -> Result<Pubkey, ScreenError> {
    Pubkey::from_str(s).map_err(|_| ScreenError::BadAddress(s.to_string()))
}

/// Deterministic fixture wallet address (base58 of `[byte; 32]`) for
/// tests and offline plumbing demos.
pub fn fixture_wallet(byte: u8) -> String {
    bs58::encode([byte; 32]).into_string()
}

/// Resolve the full account-key list for a raw message: static keys plus
/// address-table-loaded addresses (writable then readonly). `None` on any
/// undecodable key — callers fail the whole transaction rather than
/// mis-resolve indices (a wrong program match is worse than a miss).
pub(crate) fn resolve_keys(
    raw: &UiRawMessage,
    meta: &Option<UiTransactionStatusMeta>,
) -> Option<Vec<Pubkey>> {
    let mut keys: Vec<Pubkey> = Vec::with_capacity(raw.account_keys.len());
    for key in &raw.account_keys {
        keys.push(Pubkey::from_str(key).ok()?);
    }
    if let Some(meta) = meta
        && let OptionSerializer::Some(loaded) = &meta.loaded_addresses
    {
        for key in loaded.writable.iter().chain(loaded.readonly.iter()) {
            keys.push(Pubkey::from_str(key).ok()?);
        }
    }
    Some(keys)
}

/// Standard JSON transaction-fetch config (shared by all readers).
/// Accepts transaction versions through v1: current Pump.fun launches
/// are v1 envelopes, and `Some(0)` makes the RPC refuse them outright
/// (-32015). The Json wire shape decodes identically across versions
/// (proven by the mainnet `create_v2` replay fixture). A future v2
/// envelope fails deterministically here (-32015, loud) — bump then.
pub fn json_tx_config(commitment: CommitmentConfig) -> RpcTransactionConfig {
    RpcTransactionConfig {
        encoding: Some(UiTransactionEncoding::Json),
        commitment: Some(commitment),
        max_supported_transaction_version: Some(1),
    }
}

/// Single fan-out over every compiled instruction in a fetched
/// transaction (top-level plus inner compiled). DRY helper shared by
/// [`extract_pump_swaps`], [`count_dev_creates`], and
/// [`find_funding_source`] so the top/inner traversal cannot drift.
pub(crate) fn for_each_compiled_ix(
    raw: &UiRawMessage,
    meta: &Option<UiTransactionStatusMeta>,
    f: &mut impl FnMut(u8, &[u8], &str),
) {
    for cix in &raw.instructions {
        f(cix.program_id_index, &cix.accounts, &cix.data);
    }
    if let Some(meta) = meta
        && let OptionSerializer::Some(sets) = &meta.inner_instructions
    {
        for set in sets {
            for cix in &set.instructions {
                if let UiInstruction::Compiled(compiled) = cix {
                    f(
                        compiled.program_id_index,
                        &compiled.accounts,
                        &compiled.data,
                    );
                }
            }
        }
    }
}

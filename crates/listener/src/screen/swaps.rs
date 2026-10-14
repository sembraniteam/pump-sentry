use super::*;

/// Trader slot in buy/sell ixs.
pub const IDX_SWAP_USER: usize = 6;
/// Trader slot in v2 ixs: fee payer at 13.
pub const IDX_SWAP_V2_USER: usize = 13;
/// Old name of [`IDX_SWAP_V2_USER`]. Kept.
pub const IDX_BUY_V2_USER: usize = IDX_SWAP_V2_USER;
/// Trader slot in v3 ixs: position 8.
pub const IDX_SWAP_V3_USER: usize = 8;
/// Creator slot in create ixs.
pub const IDX_CREATE_USER: usize = 7;

/// Swap direction on the bonding curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapSide {
    /// Tokens bought with SOL.
    Buy,
    /// Tokens sold for SOL.
    Sell,
    /// Unmapped pump ix. Fails closed: no side, no amount.
    /// Never counted. Only the dev rule reads these.
    Unknown,
}

/// One classified swap in a tx.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapRecord {
    /// Transaction signature (base58).
    pub signature: String,
    /// Slot the tx landed in.
    pub slot: u64,
    /// Trader (v1=6, v2=13, v3=8).
    pub wallet: Pubkey,
    /// Direction by discriminator.
    pub side: SwapSide,
    /// Base units moved (0 when undecodable).
    pub amount_base_units: u64,
}

impl SwapRecord {
    /// Build from strings. Parses the wallet.
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

/// Classify every buy/sell ix into records. Covers v1, v2, v3.
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
        // (side, trader index, amount-u64 offset) per shape.
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
            // Exact-spend buys carry (spend, min_out). Use min_out.
            (SwapSide::Buy, IDX_SWAP_V3_USER, 16)
        } else {
            // Unmapped ix: `create`/`migrate` ignored.
            // Rest recorded on first account for the dev rule.
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
        // Amount is the first u64, except exact-quote v3 (second u64).
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

/// Count `create` ixs by `dev`. Powers the prior-launch check.
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
        // Author differs by version: v1 at 7, v2 at 5.
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

/// Parse a base58 address.
pub fn parse_address(s: &str) -> Result<Pubkey, ScreenError> {
    Pubkey::from_str(s).map_err(|_| ScreenError::BadAddress(s.to_string()))
}

/// Fixture wallet: base58 of `[byte; 32]`.
pub fn fixture_wallet(byte: u8) -> String {
    bs58::encode([byte; 32]).into_string()
}

/// Resolve full key list: static plus table addresses.
/// `None` on any bad key: callers drop the tx.
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

/// Standard JSON fetch config. Allows v1 envelopes.
/// New v2 envelopes fail here: bump then.
pub fn json_tx_config(commitment: CommitmentConfig) -> RpcTransactionConfig {
    RpcTransactionConfig {
        encoding: Some(UiTransactionEncoding::Json),
        commitment: Some(commitment),
        max_supported_transaction_version: Some(1),
    }
}

/// One fan-out over every compiled ix (top plus inner).
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

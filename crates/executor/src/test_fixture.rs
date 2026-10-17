//! Typed access to frozen-mainnet fixtures (unit tests only).
//!
//! Fixtures are recorded wire truth: tests decode them through the same
//! `solana-transaction-status` DTOs the prod paths use, so a shape
//! mismatch breaks loudly here instead of being hidden by hand-walking
//! untyped JSON. (The integration tests mirror the decode helpers in
//! `tests/common/mod.rs` — `cfg(test)` items are invisible to them.)

use solana_transaction_status::{
    EncodedConfirmedTransactionWithStatusMeta, EncodedTransaction, UiMessage, UiRawMessage,
    option_serializer::OptionSerializer,
};

/// Typed fixture transaction. Fills ship as *bare* `getTransaction`
/// results (no JSON-RPC envelope), so the fixture is the decode target.
/// Panics on drift (fixtures are frozen, never user input).
pub(crate) fn confirmed_tx(fixture: &str) -> EncodedConfirmedTransactionWithStatusMeta {
    serde_json::from_str(fixture).expect("fixture decodes")
}

/// Raw (unparsed) message of a JSON-encoded fixture transaction.
pub(crate) fn raw_message(fetched: &EncodedConfirmedTransactionWithStatusMeta) -> &UiRawMessage {
    match &fetched.transaction.transaction {
        EncodedTransaction::Json(ui_tx) => match &ui_tx.message {
            UiMessage::Raw(raw) => raw,
            UiMessage::Parsed(_) => {
                panic!("fixture message must be raw (parsed keys drop wire indices)")
            }
        },
        other => panic!("fixture transaction must be JSON-encoded, got {other:?}"),
    }
}

/// Fixture key table: static `accountKeys`, then loaded addresses
/// (writable, then readonly) — the raw wire order.
pub(crate) fn key_table(fetched: &EncodedConfirmedTransactionWithStatusMeta) -> Vec<String> {
    let mut keys = raw_message(fetched).account_keys.clone();
    if let Some(meta) = fetched.transaction.meta.as_ref()
        && let OptionSerializer::Some(loaded) = &meta.loaded_addresses
    {
        keys.extend(loaded.writable.iter().cloned());
        keys.extend(loaded.readonly.iter().cloned());
    }
    keys
}

/// Landed-transaction marker: `meta` present with `err == null` (a
/// failed fill proves nothing about the wire shape).
pub(crate) fn landed(fetched: &EncodedConfirmedTransactionWithStatusMeta) -> bool {
    fetched
        .transaction
        .meta
        .as_ref()
        .is_some_and(|m| m.err.is_none())
}

/// A frozen on-chain fill, decoded for byte-equality checks.
pub(crate) struct FrozenFill {
    /// Fixture key table (static keys + loaded addresses).
    pub(crate) keys: Vec<String>,
    /// Instruction account indices, wire order.
    pub(crate) idx: Vec<usize>,
    /// Decoded instruction data.
    pub(crate) data: Vec<u8>,
}

/// The fixture's `program` instruction whose data starts with `disc`
/// and has length `data_len`; panics when absent (frozen fixtures).
pub(crate) fn frozen_fill(
    fetched: &EncodedConfirmedTransactionWithStatusMeta,
    program: &solana_pubkey::Pubkey,
    disc: &[u8],
    data_len: usize,
) -> FrozenFill {
    let keys = key_table(fetched);
    let program = program.to_string();
    let pi = keys
        .iter()
        .position(|k| *k == program)
        .expect("program key");
    for ix in &raw_message(fetched).instructions {
        if usize::from(ix.program_id_index) != pi {
            continue;
        }
        let data = bs58::decode(&ix.data).into_vec().expect("b58 data");
        if data.starts_with(disc) && data.len() == data_len {
            let idx: Vec<usize> = ix.accounts.iter().map(|i| usize::from(*i)).collect();
            return FrozenFill { keys, idx, data };
        }
    }
    panic!("fixture carries no matching fill instruction");
}

/// Frozen pool account fixture `{amm, owner, data: [u8]}` (Raydium V4 /
/// PumpSwap pools, recorded via `getAccountInfo`).
#[derive(Debug, serde::Deserialize)]
pub(crate) struct PoolFixture {
    pub(crate) amm: String,
    pub(crate) owner: String,
    pub(crate) data: Vec<u8>,
}

/// Decode a frozen pool fixture; panics on drift.
pub(crate) fn pool_fixture(fixture: &str) -> PoolFixture {
    serde_json::from_str(fixture).expect("pool fixture decodes")
}

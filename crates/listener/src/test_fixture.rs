//! Typed access to the frozen-mainnet fixtures (tests only).
//!
//! Fixtures are recorded wire truth: the tests decode them through the
//! same `solana-transaction-status` DTOs the prod paths use, so a shape
//! mismatch breaks loudly here instead of being hidden by hand-walking
//! untyped JSON.

use solana_transaction_status::{
    EncodedConfirmedTransactionWithStatusMeta, EncodedTransaction, UiMessage, UiRawMessage,
    option_serializer::OptionSerializer,
};

/// RPC envelope around a `getTransaction` result (fixture files).
#[derive(serde::Deserialize)]
struct RpcEnvelope<T> {
    result: T,
}

/// Typed `result` of a frozen `getTransaction` fixture; panics on
/// drift (fixtures are frozen wire truth, never user input).
pub(crate) fn confirmed_tx(fixture: &str) -> EncodedConfirmedTransactionWithStatusMeta {
    let envelope: RpcEnvelope<EncodedConfirmedTransactionWithStatusMeta> =
        serde_json::from_str(fixture).expect("fixture envelope decodes");
    envelope.result
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

//! Entry gate plus dev screening.

use crate::parser::{
    IDX_CREATE_V2_USER, PUMP_CREATE_DISCRIMINATOR, PUMP_CREATE_V2_DISCRIMINATOR, PUMP_PROGRAM_ID,
    decode_ix_data,
};
pub use ids::{
    ASSOCIATED_TOKEN_PROGRAM_ID, MIGRATE_POOL_ACCOUNT_INDEX, MIGRATE_V2_POOL_ACCOUNT_INDEX,
    PUMP_BUY_DISCRIMINATOR, PUMP_BUY_EXACT_QUOTE_IN_V3_DISCRIMINATOR, PUMP_BUY_V2_DISCRIMINATOR,
    PUMP_BUY_V3_DISCRIMINATOR, PUMP_MIGRATE_DISCRIMINATOR, PUMP_MIGRATE_V2_DISCRIMINATOR,
    PUMP_SELL_DISCRIMINATOR, PUMP_SELL_V2_DISCRIMINATOR, PUMP_SELL_V3_DISCRIMINATOR,
    SYSTEM_PROGRAM_ID,
};
use migration::{
    MigratedPool, MigrationDestination, MigrationMonitor, PumpSwapPool, RAYDIUM_V4_PROGRAM_ID,
    RaydiumV4Pool,
};
use solana_client::{
    client_error::ClientError, nonblocking::rpc_client::RpcClient,
    rpc_client::GetConfirmedSignaturesForAddress2Config, rpc_config::RpcTransactionConfig,
    rpc_response::RpcTokenAccountBalance,
};
use solana_commitment_config::CommitmentConfig;
use solana_sdk::{pubkey::Pubkey, signature::Signature};
use solana_transaction_status::{
    EncodedConfirmedTransactionWithStatusMeta, EncodedTransaction, UiInstruction, UiMessage,
    UiRawMessage, UiTransactionEncoding, UiTransactionStatusMeta, UiTransactionTokenBalance,
    option_serializer::OptionSerializer,
};
use std::{
    collections::HashMap,
    collections::HashSet,
    future::Future,
    str::FromStr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tracing::{debug, warn};

mod bundle;
mod cache;
mod entry;
mod micro;
mod rpc;
mod swaps;
pub(crate) use cache::TimedCache;
// Re-exported by `listener::` in lib.rs; names keep their original paths.
pub use bundle::{BundleAnalysis, BundleDetector, BundleVerdict};
pub use entry::{
    DevScreenConfig, DevVerdict, EntryGate, EntryGateView, GATE_POLL_INTERVAL, GateOutcome,
    MAX_COHORT_WALLETS, MAX_PENDING_ENTRIES, judge_developer,
};
pub use micro::{MicroSignals, TapePoint, micro_signals};
pub use rpc::{
    DecodedIx, DetectedMigration, INITIALIZE2_AMM_INDEX, MAX_TRACED_BUYERS, MigrationReport,
    RpcScreener, find_funding_source, initialize2_amm_id, migrate_pool_id, read_curve_swaps,
    screen_developer,
};
pub use swaps::{
    IDX_CREATE_USER, IDX_SWAP_USER, IDX_SWAP_V2_USER, IDX_SWAP_V3_USER, SwapRecord, SwapSide,
    count_dev_creates, extract_pump_swaps, fixture_wallet, json_tx_config, parse_address,
    tape_volume_base_units,
};
/// Screening failures: config, keys, transport.
#[derive(Error, Debug)]
pub enum ScreenError {
    /// RPC endpoint URL is invalid.
    #[error("invalid rpc url: {0}")]
    BadUrl(String),
    /// Address is not a valid pubkey: {0}
    #[error("invalid address: {0}")]
    BadAddress(String),
    /// RPC transport failure.
    #[error("rpc error: {0}")]
    Rpc(#[from] ClientError),
    /// RPC read exceeded its deadline.
    #[error("rpc read timed out: {0}")]
    Timeout(&'static str),
}

#[cfg(test)]
mod tests {
    use super::rpc::{decode_migration_tx, sample_trustworthy};
    use super::*;
    use solana_transaction_status::{
        EncodedTransactionWithStatusMeta, UiCompiledInstruction, UiInnerInstructions, UiRawMessage,
        UiTransaction, UiTransactionTokenBalance,
    };

    fn key(byte: u8) -> String {
        Pubkey::new_from_array([byte; 32]).to_string()
    }

    fn buy_ix(program_idx: u8, user_idx: u8) -> UiCompiledInstruction {
        let mut data = PUMP_BUY_DISCRIMINATOR.to_vec();
        data.extend_from_slice(&[9u8; 16]);
        UiCompiledInstruction {
            program_id_index: program_idx,
            accounts: vec![20, 21, 22, 23, 24, 25, user_idx, 27],
            data: bs58::encode(data).into_string(),
            stack_height: None,
        }
    }

    fn sell_ix(program_idx: u8, user_idx: u8) -> UiCompiledInstruction {
        let mut data = PUMP_SELL_DISCRIMINATOR.to_vec();
        data.extend_from_slice(&[9u8; 16]);
        UiCompiledInstruction {
            program_id_index: program_idx,
            accounts: vec![20, 21, 22, 23, 24, 25, user_idx, 27],
            data: bs58::encode(data).into_string(),
            stack_height: None,
        }
    }

    /// Fabricated Json transaction: account 0 = pump program, account 6 = trader.
    fn swap_tx(
        instructions: Vec<UiCompiledInstruction>,
    ) -> EncodedConfirmedTransactionWithStatusMeta {
        let mut account_keys = vec![PUMP_PROGRAM_ID.to_string()];
        for b in 1u8..=27 {
            account_keys.push(key(b));
        }
        EncodedConfirmedTransactionWithStatusMeta {
            slot: 1,
            transaction: EncodedTransactionWithStatusMeta {
                transaction: EncodedTransaction::Json(UiTransaction {
                    signatures: vec!["sig".to_string()],
                    message: UiMessage::Raw(UiRawMessage {
                        header: solana_message::MessageHeader {
                            num_required_signatures: 1,
                            num_readonly_signed_accounts: 0,
                            num_readonly_unsigned_accounts: 1,
                        },
                        account_keys,
                        recent_blockhash: key(99),
                        instructions,
                        address_table_lookups: None,
                        transaction_config: None,
                    }),
                }),
                meta: None,
                version: None,
            },
            block_time: None,
            transaction_index: None,
        }
    }

    #[test]
    fn classifies_buy_and_sell() {
        // program idx 0 = pump; user idx 6 → account_keys[6] = key(6).
        let tx = swap_tx(vec![buy_ix(0, 6), sell_ix(0, 6)]);
        let records = extract_pump_swaps(&tx, "sig");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].side, SwapSide::Buy);
        assert_eq!(records[1].side, SwapSide::Sell);
        assert_eq!(records[0].wallet, Pubkey::new_from_array([6; 32]));
    }

    #[test]
    fn classifies_buy_v2_with_trader_at_index_13() {
        // buy_v2 layout: trader (fee payer) at account position 13.
        let mut data = PUMP_BUY_V2_DISCRIMINATOR.to_vec();
        data.extend_from_slice(&[7u8; 16]);
        let v2_ix = UiCompiledInstruction {
            program_id_index: 0,
            accounts: (0u8..27).collect(),
            data: bs58::encode(data).into_string(),
            stack_height: None,
        };
        let tx = swap_tx(vec![v2_ix]);
        let records = extract_pump_swaps(&tx, "sig");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].side, SwapSide::Buy);
        // account_keys[13] = key(13) in the fabricated tx.
        assert_eq!(records[0].wallet, Pubkey::new_from_array([13; 32]));
        assert_eq!(records[0].amount_base_units, u64::from_le_bytes([7; 8]));
    }

    #[test]
    fn classifies_v3_shapes_with_trader_at_index_8() {
        // v3 layout (17-account table): trader at position 8; exact-quote
        // attributes `min_tokens_out` (second u64, conservative bound).
        let cases = [
            (PUMP_BUY_V3_DISCRIMINATOR.to_vec(), [7u8; 16], SwapSide::Buy),
            (
                PUMP_SELL_V3_DISCRIMINATOR.to_vec(),
                [7u8; 16],
                SwapSide::Sell,
            ),
        ];
        for (disc, payload, want_side) in cases {
            let mut data = disc;
            data.extend_from_slice(&payload);
            let v3_ix = UiCompiledInstruction {
                program_id_index: 0,
                accounts: (0u8..17).collect(),
                data: bs58::encode(data).into_string(),
                stack_height: None,
            };
            let tx = swap_tx(vec![v3_ix]);
            let records = extract_pump_swaps(&tx, "sig");
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].side, want_side);
            // account_keys[8] = key(8) in the fabricated tx.
            assert_eq!(records[0].wallet, Pubkey::new_from_array([8; 32]));
            assert_eq!(records[0].amount_base_units, u64::from_le_bytes([7; 8]));
        }
        // Exact-quote: (spendable_quote_in, min_tokens_out).
        let mut data = PUMP_BUY_EXACT_QUOTE_IN_V3_DISCRIMINATOR.to_vec();
        data.extend_from_slice(&[9u8; 8]);
        data.extend_from_slice(&[3u8; 8]);
        let exact_ix = UiCompiledInstruction {
            program_id_index: 0,
            accounts: (0u8..17).collect(),
            data: bs58::encode(data).into_string(),
            stack_height: None,
        };
        let tx = swap_tx(vec![exact_ix]);
        let records = extract_pump_swaps(&tx, "sig");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].side, SwapSide::Buy);
        assert_eq!(records[0].wallet, Pubkey::new_from_array([8; 32]));
        assert_eq!(records[0].amount_base_units, u64::from_le_bytes([3; 8]));
    }

    #[test]
    fn replay_mainnet_buy_v2_swap() {
        // Frozen mainnet `buy_v2`: trader resolves to the fee payer with
        // the exact on-chain amount (3_485_709_524_202 base units).
        let fetched = crate::test_fixture::confirmed_tx(include_str!(
            "../../tests/fixtures/create_v2_mainnet.json"
        ));
        let records = extract_pump_swaps(&fetched, "sig");
        let buys: Vec<_> = records.iter().filter(|r| r.side == SwapSide::Buy).collect();
        assert_eq!(buys.len(), 1);
        assert_eq!(
            buys[0].wallet.to_string(),
            "DC4X9szDJk2wVm2NAGsshGFydKAahBg6sAAjmxvSJZGh"
        );
        assert_eq!(buys[0].amount_base_units, 3_485_709_524_202);
    }

    #[test]
    fn replay_mainnet_buy_v2_amount_matches_balance_delta() {
        // Independent proof: the buyer's token-balance delta equals the
        // classified amount to the unit.
        let fetched = crate::test_fixture::confirmed_tx(include_str!(
            "../../tests/fixtures/create_v2_mainnet.json"
        ));
        let records = extract_pump_swaps(&fetched, "sig");
        let buy = records
            .iter()
            .find(|r| r.side == SwapSide::Buy)
            .expect("one buy");
        let meta = fetched.transaction.meta.as_ref().expect("fixture meta");
        let buyer = buy.wallet.to_string();
        let delta = |balances: &OptionSerializer<Vec<UiTransactionTokenBalance>>| -> i128 {
            match balances {
                OptionSerializer::Some(list) => list
                    .iter()
                    .filter(|b| matches!(&b.owner, OptionSerializer::Some(o) if o == &buyer))
                    .map(|b| b.ui_token_amount.amount.parse::<i128>().unwrap_or(0))
                    .sum(),
                _ => 0,
            }
        };
        let net = delta(&meta.post_token_balances) - delta(&meta.pre_token_balances);
        assert_eq!(net, buy.amount_base_units as i128);
        assert_eq!(net, 3_485_709_524_202);
    }

    #[test]
    fn replay_mainnet_buy_v2_second_sample() {
        // Second independent `buy_v2`: same attribution, amount matches
        // the balance delta to the unit (8_671_591_758_466).
        let fetched = crate::test_fixture::confirmed_tx(include_str!(
            "../../tests/fixtures/buy_v2_second_mainnet.json"
        ));
        let records = extract_pump_swaps(&fetched, "sig");
        let buys: Vec<_> = records.iter().filter(|r| r.side == SwapSide::Buy).collect();
        assert_eq!(buys.len(), 1);
        assert_eq!(
            buys[0].wallet.to_string(),
            "6VrR1eTzeEj6eubVpxAQbHFqJ6fQHUnMqxsCEhg7kgrj"
        );
        assert_eq!(buys[0].amount_base_units, 8_671_591_758_466);
    }

    #[test]
    fn replay_mainnet_sell_v2_swap() {
        // Frozen mainnet `sell_v2`: trader resolves to the fee payer with
        // the exact on-chain amount (balance-delta cross-checked off-test).
        let fetched = crate::test_fixture::confirmed_tx(include_str!(
            "../../tests/fixtures/sell_v2_mainnet.json"
        ));
        assert_eq!(fetched.slot, 455676914);
        let records = extract_pump_swaps(&fetched, "sig");
        let sells: Vec<_> = records
            .iter()
            .filter(|r| r.side == SwapSide::Sell)
            .collect();
        assert_eq!(sells.len(), 1);
        assert_eq!(
            sells[0].wallet.to_string(),
            "Bck5r87KG8Ak9WhyGQpHn5SqvyuB6tTHQAMCzoxjsBU2"
        );
        assert_eq!(sells[0].amount_base_units, 3_060_466_246_688);
    }

    #[test]
    fn replay_mainnet_sell_v2_second_sample() {
        // Second independent `sell_v2` (SOL-quoted): amount matches the
        // seller's token-balance delta to the unit (1_679_259_586_074).
        let fetched = crate::test_fixture::confirmed_tx(include_str!(
            "../../tests/fixtures/sell_v2_second_mainnet.json"
        ));
        assert_eq!(fetched.slot, 455844140);
        let records = extract_pump_swaps(&fetched, "sig");
        let sells: Vec<_> = records
            .iter()
            .filter(|r| r.side == SwapSide::Sell)
            .collect();
        assert_eq!(sells.len(), 1);
        assert_eq!(
            sells[0].wallet.to_string(),
            "GiverXMTjyePxBFbgPDareo3atUfvMosDd8di1uMgUBq"
        );
        assert_eq!(sells[0].amount_base_units, 1_679_259_586_074);
    }

    #[test]
    fn cohort_wallets_dump_abort_but_never_count() {
        // v2 creator cohort: their sells abort like the launcher's, and
        // their buys never count toward quorum.
        let mut gate = EntryGate::new(60, 2);
        let dev = Pubkey::new_from_array([7; 32]);
        let creator = Pubkey::new_from_array([8; 32]);
        assert!(gate.submit("MINT", &dev.to_string(), 1000));
        assert!(gate.add_cohort("MINT", &creator.to_string()));
        // Duplicate / unknown cohort joins fail (bounded, explicit).
        assert!(!gate.add_cohort("MINT", &creator.to_string()));
        assert!(!gate.add_cohort("MINT", "not-a-key"));
        assert!(!gate.add_cohort("NOPE", &creator.to_string()));
        // Creator buying does not count toward quorum.
        let cohort_buy = SwapRecord {
            signature: "c1".to_string(),
            slot: 1001,
            wallet: creator,
            side: SwapSide::Buy,
            amount_base_units: 1_000,
        };
        assert_eq!(gate.observe("MINT", &[cohort_buy], 1001), GateOutcome::Wait);
        // Creator selling aborts immediately.
        let dump = SwapRecord {
            signature: "c2".to_string(),
            slot: 1002,
            wallet: creator,
            side: SwapSide::Sell,
            amount_base_units: 1_000,
        };
        match gate.observe("MINT", &[dump], 1002) {
            GateOutcome::Abort { reason } => assert!(reason.contains("dev dump")),
            other => panic!("expected abort, got {other:?}"),
        }
        assert_eq!(gate.pending_count(), 0);
    }

    #[test]
    fn sell_v2_dev_dump_aborts_the_gate() {
        // A classified `sell_v2` authored by the dev wallet aborts like
        // any legacy dev dump (layout now verified, no more blindness).
        let mut gate = EntryGate::new(60, 2);
        let dev = Pubkey::new_from_array([7; 32]);
        assert!(gate.submit("MINT", &dev.to_string(), 1000));
        let dump = SwapRecord {
            signature: "v2dump".to_string(),
            slot: 1003,
            wallet: dev,
            side: SwapSide::Sell,
            amount_base_units: 1_000,
        };
        match gate.observe("MINT", &[dump], 1003) {
            GateOutcome::Abort { reason } => assert!(reason.contains("dev dump")),
            other => panic!("expected abort, got {other:?}"),
        }
        assert_eq!(gate.pending_count(), 0);
    }

    #[test]
    fn ignores_non_pump_program() {
        let foreign = buy_ix(5, 6); // program idx 5 = key(5), not pump
        let tx = swap_tx(vec![foreign]);
        assert!(extract_pump_swaps(&tx, "sig").is_empty());
    }

    #[test]
    fn unknown_pump_ix_is_recorded_but_never_counts() {
        // Unmapped discriminators surface as `Unknown` against the ix's
        // first listed account — never quorum-counted, amount always 0.
        let mut unknown = buy_ix(0, 6);
        unknown.data = bs58::encode([0u8; 24]).into_string();
        let tx = swap_tx(vec![unknown]);
        let records = extract_pump_swaps(&tx, "sig");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].side, SwapSide::Unknown);
        // buy_ix lists account 20 first → key(20) in the fabricated tx.
        assert_eq!(records[0].wallet, Pubkey::new_from_array([20; 32]));
        assert_eq!(records[0].amount_base_units, 0);
    }

    #[test]
    fn count_dev_creates_uses_versioned_author_index() {
        // v2 `user` sits at index 5, v1 at 7: one shared index
        // undercounts v2 serial launchers and misattributes v1-adjacent ones.
        let mk = |disc: &[u8; 8]| {
            let mut data = disc.to_vec();
            data.extend_from_slice(&[9u8; 16]);
            UiCompiledInstruction {
                program_id_index: 0,
                // Identity mapping: position i resolves to key(i), so
                // position 5 is the v2 author and 7 the v1 author.
                accounts: (0u8..14).collect(),
                data: bs58::encode(data).into_string(),
                stack_height: None,
            }
        };
        let v1 = mk(&PUMP_CREATE_DISCRIMINATOR);
        let v2 = mk(&PUMP_CREATE_V2_DISCRIMINATOR);
        let tx_v1 = swap_tx(vec![v1]);
        let tx_v2 = swap_tx(vec![v2]);
        // Position 5 → key(5) (v2 author); position 7 → key(7) (v1 author).
        assert_eq!(
            count_dev_creates(&tx_v1, &Pubkey::new_from_array([7; 32])),
            Some(1)
        );
        assert_eq!(
            count_dev_creates(&tx_v1, &Pubkey::new_from_array([5; 32])),
            Some(0)
        );
        assert_eq!(
            count_dev_creates(&tx_v2, &Pubkey::new_from_array([5; 32])),
            Some(1)
        );
        assert_eq!(
            count_dev_creates(&tx_v2, &Pubkey::new_from_array([7; 32])),
            Some(0)
        );
    }

    #[test]
    fn create_and_migrate_ixs_stay_ignored() {
        // Benign lifecycle ixs must never abort the gate.
        let mk = |disc: &[u8; 8]| {
            let mut ix = buy_ix(0, 6);
            let mut data = disc.to_vec();
            data.extend_from_slice(&[9u8; 16]);
            ix.data = bs58::encode(data).into_string();
            ix
        };
        let tx = swap_tx(vec![
            mk(&PUMP_CREATE_DISCRIMINATOR),
            mk(&PUMP_CREATE_V2_DISCRIMINATOR),
            mk(&PUMP_MIGRATE_DISCRIMINATOR),
        ]);
        assert!(extract_pump_swaps(&tx, "sig").is_empty());
    }

    #[test]
    fn gate_rejects_bad_dev_address() {
        let mut gate = EntryGate::new(60, 3);
        assert!(!gate.submit("MINT", "not-a-key", 1000));
        assert_eq!(gate.pending_count(), 0);
    }

    #[test]
    fn unreadable_sample_majority_is_untrustworthy() {
        // Empty sample proves nothing: fail closed, never count zero.
        assert!(!sample_trustworthy(0, 0));
        assert!(sample_trustworthy(0, 20));
        assert!(sample_trustworthy(1, 20));
        assert!(sample_trustworthy(9, 20));
        // Half or more unreadable: fail closed.
        assert!(!sample_trustworthy(10, 20));
        assert!(!sample_trustworthy(20, 20));
        assert!(!sample_trustworthy(1, 1));
    }

    #[test]
    fn watchlist_evicts_oldest_past_cap() {
        let mut gate = EntryGate::new(60, 3);
        let dev = Pubkey::new_from_array([7; 32]).to_string();
        for i in 0..(MAX_PENDING_ENTRIES + 5) {
            assert!(gate.submit(&format!("MINT{i:05}"), &dev, 1000));
        }
        assert_eq!(gate.pending_count(), MAX_PENDING_ENTRIES);
    }

    #[test]
    fn tape_volume_sums_both_sides_saturating() {
        let rec = |side: SwapSide, amount: u64| SwapRecord {
            signature: "s".to_string(),
            slot: 1001,
            wallet: Pubkey::new_from_array([11; 32]),
            side,
            amount_base_units: amount,
        };
        // Both sides count; undecodable (0-amount) records add nothing.
        assert_eq!(
            tape_volume_base_units(&[
                rec(SwapSide::Buy, 1_000_000),
                rec(SwapSide::Sell, 500_000),
                rec(SwapSide::Unknown, 0),
            ]),
            1_500_000
        );
        assert_eq!(tape_volume_base_units(&[]), 0);
        // Adversarial tape never wraps: saturation caps the proxy.
        assert_eq!(
            tape_volume_base_units(&[rec(SwapSide::Buy, u64::MAX), rec(SwapSide::Buy, 1)]),
            u64::MAX
        );
    }

    #[test]
    fn gate_confirms_on_quorum() {
        let mut gate = EntryGate::new(60, 3);
        let dev = Pubkey::new_from_array([7; 32]);
        assert!(gate.submit("MINT", &dev.to_string(), 1000));
        let buyers = [11u8, 12, 13].map(|b| SwapRecord {
            signature: format!("s{b}"),
            slot: 1001,
            wallet: Pubkey::new_from_array([b; 32]),
            side: SwapSide::Buy,
            amount_base_units: 1_000,
        });
        assert_eq!(gate.observe("MINT", &buyers[..2], 1001), GateOutcome::Wait);
        assert_eq!(
            gate.observe("MINT", &buyers[2..], 1002),
            GateOutcome::Confirm { buyers: 3 }
        );
        assert_eq!(gate.pending_count(), 0);
    }

    #[test]
    fn gate_ignores_dev_buys_and_aborts_dev_dump() {
        let mut gate = EntryGate::new(60, 2);
        let dev = Pubkey::new_from_array([7; 32]);
        assert!(gate.submit("MINT", &dev.to_string(), 1000));
        // Dev buying does not count toward quorum.
        let dev_buy = SwapRecord {
            signature: "s1".to_string(),
            slot: 1001,
            wallet: dev,
            side: SwapSide::Buy,
            amount_base_units: 1_000,
        };
        assert_eq!(gate.observe("MINT", &[dev_buy], 1001), GateOutcome::Wait);
        // One real buyer is still below quorum of 2.
        let buy = SwapRecord {
            signature: "s2".to_string(),
            slot: 1002,
            wallet: Pubkey::new_from_array([11; 32]),
            side: SwapSide::Buy,
            amount_base_units: 1_000,
        };
        assert_eq!(gate.observe("MINT", &[buy], 1002), GateOutcome::Wait);
        // Dev selling aborts immediately.
        let dump = SwapRecord {
            signature: "s3".to_string(),
            slot: 1003,
            wallet: dev,
            side: SwapSide::Sell,
            amount_base_units: 1_000,
        };
        match gate.observe("MINT", &[dump], 1003) {
            GateOutcome::Abort { reason } => assert!(reason.contains("dev dump")),
            other => panic!("expected abort, got {other:?}"),
        }
        assert_eq!(gate.pending_count(), 0);
    }

    #[test]
    fn gate_aborts_on_dev_unknown_ix_but_ignores_strangers() {
        let mut gate = EntryGate::new(60, 2);
        let dev = Pubkey::new_from_array([7; 32]);
        assert!(gate.submit("MINT", &dev.to_string(), 1000));
        // Unknown ix from a stranger: no quorum credit, no abort.
        let strange = SwapRecord {
            signature: "u1".to_string(),
            slot: 1001,
            wallet: Pubkey::new_from_array([11; 32]),
            side: SwapSide::Unknown,
            amount_base_units: 0,
        };
        assert_eq!(gate.observe("MINT", &[strange], 1001), GateOutcome::Wait);
        // One real buyer is still below quorum of 2 (unknown added none).
        let buy = SwapRecord {
            signature: "s2".to_string(),
            slot: 1002,
            wallet: Pubkey::new_from_array([12; 32]),
            side: SwapSide::Buy,
            amount_base_units: 1_000,
        };
        assert_eq!(gate.observe("MINT", &[buy], 1002), GateOutcome::Wait);
        // Unknown ix naming the dev wallet: fail-closed abort.
        let dev_unknown = SwapRecord {
            signature: "u2".to_string(),
            slot: 1003,
            wallet: dev,
            side: SwapSide::Unknown,
            amount_base_units: 0,
        };
        match gate.observe("MINT", &[dev_unknown], 1003) {
            GateOutcome::Abort { reason } => assert!(reason.contains("unknown pump")),
            other => panic!("expected abort, got {other:?}"),
        }
        assert_eq!(gate.pending_count(), 0);
    }

    #[test]
    fn gate_times_out_and_sweeps() {
        let mut gate = EntryGate::new(10, 3);
        let dev = Pubkey::new_from_array([7; 32]);
        assert!(gate.submit("A", &dev.to_string(), 1000));
        assert!(gate.submit("B", &dev.to_string(), 1000));
        let buy = SwapRecord {
            signature: "s".to_string(),
            slot: 1005,
            wallet: Pubkey::new_from_array([11; 32]),
            side: SwapSide::Buy,
            amount_base_units: 1_000,
        };
        assert_eq!(gate.observe("A", &[buy], 1005), GateOutcome::Wait);
        assert!(matches!(
            gate.observe("A", &[], 1011),
            GateOutcome::Abort { .. }
        ));
        let swept = gate.sweep_timeouts(1011);
        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].0, "B");
        assert_eq!(gate.observe("UNKNOWN", &[], 1011), GateOutcome::Wait);
    }

    #[test]
    fn bundle_organic_launch_is_safe() {
        let swaps = vec![
            (100u64, Pubkey::new_from_array([11; 32]), 2.0),
            (101u64, Pubkey::new_from_array([12; 32]), 1.5),
        ];
        let analysis = BundleDetector::evaluate_launch_slot(100, &swaps, &[]);
        assert!(!analysis.is_bundled);
        assert_eq!(analysis.creation_slot_buys, 1);
        assert_eq!(analysis.verdict, BundleVerdict::SafeOrganic);
    }

    #[test]
    fn bundle_three_creation_slot_buys_is_suspect() {
        let swaps: Vec<(u64, Pubkey, f64)> = (21..=23)
            .map(|b| (200u64, Pubkey::new_from_array([b; 32]), 3.0))
            .collect();
        let analysis = BundleDetector::evaluate_launch_slot(200, &swaps, &[]);
        assert!(analysis.is_bundled);
        assert_eq!(analysis.creation_slot_buys, 3);
        assert_eq!(analysis.verdict, BundleVerdict::SuspectLowVolume);
    }

    #[test]
    fn bundle_monopoly_rejects_over_15_percent() {
        let swaps: Vec<(u64, Pubkey, f64)> = (21..=23)
            .map(|b| (300u64, Pubkey::new_from_array([b; 32]), 6.0))
            .collect();
        let analysis = BundleDetector::evaluate_launch_slot(300, &swaps, &[]);
        assert!(analysis.is_bundled);
        assert!((analysis.total_cluster_supply_pct - 18.0).abs() < 1e-9);
        assert_eq!(analysis.verdict, BundleVerdict::RejectBundledMonopoly);
    }

    #[test]
    fn bundle_supply_boundary_is_strict() {
        // Exactly 15.0% does NOT reject (skill: strictly greater).
        let swaps: Vec<(u64, Pubkey, f64)> = (21..=23)
            .map(|b| (400u64, Pubkey::new_from_array([b; 32]), 5.0))
            .collect();
        let analysis = BundleDetector::evaluate_launch_slot(400, &swaps, &[]);
        assert!(analysis.is_bundled);
        assert_eq!(analysis.verdict, BundleVerdict::SuspectLowVolume);
    }

    #[test]
    fn bundle_shared_funder_proves_coordination() {
        // Different slots, but two buyers share one genesis funder.
        let swaps = vec![
            (500u64, Pubkey::new_from_array([11; 32]), 1.0),
            (501u64, Pubkey::new_from_array([12; 32]), 1.0),
        ];
        let funders = vec![
            (
                Pubkey::new_from_array([11; 32]),
                Pubkey::new_from_array([99; 32]),
            ),
            (
                Pubkey::new_from_array([12; 32]),
                Pubkey::new_from_array([99; 32]),
            ),
            (
                Pubkey::new_from_array([13; 32]),
                Pubkey::new_from_array([98; 32]),
            ),
        ];
        let analysis = BundleDetector::evaluate_launch_slot(500, &swaps, &funders);
        assert!(analysis.is_bundled);
        assert_eq!(analysis.shared_funder_count, 2);
        assert_eq!(analysis.verdict, BundleVerdict::SuspectLowVolume);
    }

    #[test]
    fn bundle_two_same_slot_buys_are_not_enough() {
        let swaps = vec![
            (600u64, Pubkey::new_from_array([11; 32]), 1.0),
            (600u64, Pubkey::new_from_array([12; 32]), 1.0),
        ];
        let analysis = BundleDetector::evaluate_launch_slot(600, &swaps, &[]);
        assert!(!analysis.is_bundled);
        assert_eq!(analysis.verdict, BundleVerdict::SafeOrganic);
    }

    #[test]
    fn bundle_empty_input_is_safe() {
        let analysis = BundleDetector::evaluate_launch_slot(0, &[], &[]);
        assert!(!analysis.is_bundled);
        assert_eq!(analysis.creation_slot_buys, 0);
        assert_eq!(analysis.total_cluster_supply_pct, 0.0);
        assert_eq!(analysis.verdict, BundleVerdict::SafeOrganic);
    }

    #[test]
    fn funding_source_found_in_top_level_transfer() {
        // System program at index 0, from=key(1), to=key(2), 12-byte payload.
        let mut data = 2u32.to_le_bytes().to_vec();
        data.extend_from_slice(&500u64.to_le_bytes());
        let transfer = UiCompiledInstruction {
            program_id_index: 0,
            accounts: vec![1, 2],
            data: bs58::encode(data).into_string(),
            stack_height: None,
        };
        let mut account_keys = vec![SYSTEM_PROGRAM_ID.to_string()];
        for b in 1u8..=4 {
            account_keys.push(key(b));
        }
        let tx = EncodedConfirmedTransactionWithStatusMeta {
            slot: 9,
            transaction: EncodedTransactionWithStatusMeta {
                transaction: EncodedTransaction::Json(UiTransaction {
                    signatures: vec!["sig".to_string()],
                    message: UiMessage::Raw(UiRawMessage {
                        header: solana_message::MessageHeader {
                            num_required_signatures: 1,
                            num_readonly_signed_accounts: 0,
                            num_readonly_unsigned_accounts: 1,
                        },
                        account_keys,
                        recent_blockhash: key(99),
                        instructions: vec![transfer],
                        address_table_lookups: None,
                        transaction_config: None,
                    }),
                }),
                meta: None,
                version: None,
            },
            block_time: None,
            transaction_index: None,
        };
        let beneficiary = Pubkey::new_from_array([2; 32]);
        assert_eq!(
            find_funding_source(&tx, &beneficiary),
            Some(Pubkey::new_from_array([1; 32]))
        );
        // Wrong beneficiary: no match.
        assert_eq!(
            find_funding_source(&tx, &Pubkey::new_from_array([3; 32])),
            None
        );
    }

    #[test]
    fn funding_source_found_in_create_account() {
        // System createAccount: u32 LE 0 ++ 48 bytes, funder idx 0, new idx 1.
        let mut data = 0u32.to_le_bytes().to_vec();
        data.extend_from_slice(&[7u8; 48]);
        let create = UiCompiledInstruction {
            program_id_index: 0,
            accounts: vec![1, 2],
            data: bs58::encode(data).into_string(),
            stack_height: None,
        };
        let mut account_keys = vec![SYSTEM_PROGRAM_ID.to_string()];
        for b in 1u8..=4 {
            account_keys.push(key(b));
        }
        let tx = EncodedConfirmedTransactionWithStatusMeta {
            slot: 9,
            transaction: EncodedTransactionWithStatusMeta {
                transaction: EncodedTransaction::Json(UiTransaction {
                    signatures: vec!["sig".to_string()],
                    message: UiMessage::Raw(UiRawMessage {
                        header: solana_message::MessageHeader {
                            num_required_signatures: 1,
                            num_readonly_signed_accounts: 0,
                            num_readonly_unsigned_accounts: 1,
                        },
                        account_keys,
                        recent_blockhash: key(99),
                        instructions: vec![create],
                        address_table_lookups: None,
                        transaction_config: None,
                    }),
                }),
                meta: None,
                version: None,
            },
            block_time: None,
            transaction_index: None,
        };
        assert_eq!(
            find_funding_source(&tx, &Pubkey::new_from_array([2; 32])),
            Some(Pubkey::new_from_array([1; 32]))
        );
    }

    #[test]
    fn swap_records_carry_slot_and_amount() {
        let tx = swap_tx(vec![buy_ix(0, 6)]);
        let records = extract_pump_swaps(&tx, "sig");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].slot, 1);
        assert_eq!(records[0].amount_base_units, u64::from_le_bytes([9; 8]));
    }

    #[test]
    fn timed_cache_hits_misses_and_expires() {
        let mut cache: TimedCache<String> = TimedCache::new(100);
        assert_eq!(cache.get("k", 1000), None);
        cache.put("k".to_string(), "v".to_string(), 1000);
        assert_eq!(cache.get("k", 1000), Some("v".to_string()));
        assert_eq!(cache.get("k", 1100), Some("v".to_string()));
        assert_eq!(cache.get("k", 1101), None);
        assert_eq!(cache.get("other", 1000), None);
    }

    #[test]
    fn initialize2_resolves_amm_at_index_4() {
        // Wire order per raydium-amm `initialize2`: [0]=SPL Token program,
        // [1]=ATA program, [2]=system, [3]=rent, [4]=new AMM pool.
        let amm = Pubkey::new_from_array([77; 32]);
        let other = Pubkey::new_from_array([78; 32]);
        let spl_token = Pubkey::new_from_array([10; 32]);
        let ata_program = Pubkey::new_from_array([11; 32]);
        let system = Pubkey::new_from_array([12; 32]);
        let rent = Pubkey::new_from_array([13; 32]);
        let ixs = vec![
            (
                Pubkey::new_from_array([1; 32]),
                vec![Pubkey::new_from_array([2; 32])],
            ),
            (
                RAYDIUM_V4_PROGRAM_ID,
                vec![spl_token, ata_program, system, rent, amm, other],
            ),
        ];
        assert_eq!(initialize2_amm_id(&ixs), Some(amm));
        // No Raydium instruction: no AMM.
        let plain = vec![(Pubkey::new_from_array([1; 32]), vec![amm])];
        assert_eq!(initialize2_amm_id(&plain), None);
        // Truncated account list (fewer than 5): no AMM.
        let short = vec![(RAYDIUM_V4_PROGRAM_ID, vec![Pubkey::new_from_array([3; 32])])];
        assert_eq!(initialize2_amm_id(&short), None);
    }

    #[test]
    fn migration_report_flattens() {
        let detected = DetectedMigration {
            signature: "sig".to_string(),
            destination: MigrationDestination::PumpSwap,
            amm_id: None,
            pool: None,
        };
        let report = detected.report();
        assert_eq!(report.destination, MigrationDestination::PumpSwap);
        assert_eq!(report.signature, "sig");
        assert_eq!(report.amm_id, None);
    }

    #[test]
    fn replay_real_migrate_v2_fixture() {
        // Real mainnet MigrateV2 (PumpSwap) transaction, fetched once and
        // frozen: proves the live wire shape decodes and classifies.
        let fetched = crate::test_fixture::confirmed_tx(include_str!(
            "../../tests/fixtures/migrate_v2_pumpswap.json"
        ));
        assert_eq!(fetched.slot, 455487811);
        let Some((keys, decoded, logs)) = decode_migration_tx(&fetched) else {
            panic!("fixture must decode");
        };
        assert!(logs.iter().any(|l| l.contains("MigrateV2")));
        let ix_datas: Vec<Vec<u8>> = decoded.iter().map(|ix| ix.data.clone()).collect();
        // Linkage inputs: the migration's own token accounts (pump-
        // suffixed mint + curve), both present among the tx keys.
        let pump_keys: Vec<Pubkey> = keys
            .iter()
            .filter(|k| k.to_string().ends_with("pump"))
            .copied()
            .collect();
        assert!(pump_keys.len() >= 2, "fixture must carry mint + curve");
        assert_eq!(
            MigrationMonitor::new().classify_migration(
                &keys,
                &ix_datas,
                &logs,
                &pump_keys[0],
                &pump_keys[1]
            ),
            Some(MigrationDestination::PumpSwap)
        );
    }

    #[test]
    fn migrate_pool_extracts_index_10() {
        // Same frozen MigrateV2 tx: the pool sits at accounts[10] of
        // the pump `migrate_v2` ix (per pump IDL), proven by the live
        // pool account it points at.
        let fetched = crate::test_fixture::confirmed_tx(include_str!(
            "../../tests/fixtures/migrate_v2_pumpswap.json"
        ));
        let Some((_, decoded, _)) = decode_migration_tx(&fetched) else {
            panic!("fixture must decode");
        };
        assert_eq!(
            migrate_pool_id(&decoded).map(|p| p.to_string()).as_deref(),
            Some("GeWJCTs9HW5KP7N4mkovNgRjCEszntTb7czmHtXbRrCK")
        );
        // Non-migrate pump ixs and short account lists yield nothing.
        let other = vec![DecodedIx {
            program: PUMP_PROGRAM_ID,
            accounts: vec![Pubkey::new_from_array([1; 32])],
            data: vec![0u8; 8],
        }];
        assert_eq!(migrate_pool_id(&other), None);
        assert_eq!(migrate_pool_id(&[]), None);
    }

    #[test]
    fn judge_rejects_young_serial_and_accepts_clean() {
        let cfg = DevScreenConfig::default();
        assert!(!judge_developer(None, 0, &cfg).pass);
        let young = judge_developer(Some(600), 0, &cfg);
        assert!(!young.pass);
        assert!(young.reason.contains("600"));
        let serial = judge_developer(Some(7200), 3, &cfg);
        assert!(!serial.pass);
        assert!(serial.reason.contains("3"));
        let clean = judge_developer(Some(7200), 1, &cfg);
        assert!(clean.pass);
    }

    #[test]
    fn funding_source_short_payload_never_panics() {
        // `raw[..4]` on a <4-byte system ix must return None, not panic.
        for short in [vec![], vec![2u8], vec![2u8, 0, 0]] {
            let ix = UiCompiledInstruction {
                program_id_index: 0,
                accounts: vec![1, 2],
                data: bs58::encode(&short).into_string(),
                stack_height: None,
            };
            let mut account_keys = vec![SYSTEM_PROGRAM_ID.to_string()];
            for b in 1u8..=4 {
                account_keys.push(key(b));
            }
            let tx = EncodedConfirmedTransactionWithStatusMeta {
                slot: 9,
                transaction: EncodedTransactionWithStatusMeta {
                    transaction: EncodedTransaction::Json(UiTransaction {
                        signatures: vec!["sig".to_string()],
                        message: UiMessage::Raw(UiRawMessage {
                            header: solana_message::MessageHeader {
                                num_required_signatures: 1,
                                num_readonly_signed_accounts: 0,
                                num_readonly_unsigned_accounts: 1,
                            },
                            account_keys,
                            recent_blockhash: key(99),
                            instructions: vec![ix],
                            address_table_lookups: None,
                            transaction_config: None,
                        }),
                    }),
                    meta: None,
                    version: None,
                },
                block_time: None,
                transaction_index: None,
            };
            assert_eq!(
                find_funding_source(&tx, &Pubkey::new_from_array([2; 32])),
                None,
                "short payload {short:?} must be None"
            );
        }
    }

    #[test]
    fn gate_relaunch_refresh_is_capped() {
        // Regression: spammed relaunches must not extend the window forever.
        let mut gate = EntryGate::new(60, 3);
        let dev = Pubkey::new_from_array([7; 32]);
        assert!(gate.submit("MINT", &dev.to_string(), 1000));
        // Spam refresh every 10s for a long time: deadline caps at
        // created + 3×window = 1000 + 180 = 1180.
        for t in (1010..10_000).step_by(10) {
            assert!(gate.submit("MINT", &dev.to_string(), t as u64));
        }
        // Past the cap the entry must have timed out.
        assert!(matches!(
            gate.observe("MINT", &[], 1181),
            GateOutcome::Abort { .. }
        ));
        assert_eq!(gate.pending_count(), 0);
    }

    #[test]
    fn relaunch_preserves_cohort_wallets() {
        // Regression: relaunch refresh must not drop the v2 creator cohort.
        let mut gate = EntryGate::new(60, 3);
        let dev = Pubkey::new_from_array([7; 32]);
        let creator = Pubkey::new_from_array([9; 32]);
        assert!(gate.submit("MINT", &dev.to_string(), 1000));
        assert!(gate.add_cohort("MINT", &creator.to_string()));
        assert!(gate.submit("MINT", &dev.to_string(), 1010));
        // Creator sell still aborts after the relaunch.
        let dump = SwapRecord {
            wallet: creator,
            side: SwapSide::Sell,
            signature: "sig".to_string(),
            slot: 1,
            amount_base_units: 1,
        };
        assert!(matches!(
            gate.observe("MINT", &[dump], 1011),
            GateOutcome::Abort { .. }
        ));
    }

    #[test]
    fn timed_cache_prunes_expired_on_put() {
        let mut cache: TimedCache<String> = TimedCache::new(100);
        cache.put("old".to_string(), "v".to_string(), 1000);
        // Inserting far in the future must evict the stale entry.
        cache.put("new".to_string(), "v".to_string(), 5000);
        assert_eq!(cache.get("old", 5000), None);
        assert_eq!(cache.get("new", 5000), Some("v".to_string()));
    }

    /// Build a fabricated transaction from explicit keys, instructions,
    /// and an optional meta.
    fn funding_tx_with_meta(
        account_keys: &[String],
        instructions: Vec<UiCompiledInstruction>,
        meta: Option<UiTransactionStatusMeta>,
    ) -> EncodedConfirmedTransactionWithStatusMeta {
        EncodedConfirmedTransactionWithStatusMeta {
            slot: 9,
            transaction: EncodedTransactionWithStatusMeta {
                transaction: EncodedTransaction::Json(UiTransaction {
                    signatures: vec!["sig".to_string()],
                    message: UiMessage::Raw(UiRawMessage {
                        header: solana_message::MessageHeader {
                            num_required_signatures: 1,
                            num_readonly_signed_accounts: 0,
                            num_readonly_unsigned_accounts: 1,
                        },
                        account_keys: account_keys.to_vec(),
                        recent_blockhash: key(99),
                        instructions,
                        address_table_lookups: None,
                        transaction_config: None,
                    }),
                }),
                meta,
                version: None,
            },
            block_time: None,
            transaction_index: None,
        }
    }

    #[test]
    fn funding_source_found_via_transfer_with_seed() {
        // TransferWithSeed (tag 11, [from, base, to]), hand-encoded per
        // the SDK constructor source.
        let mut data = 11u32.to_le_bytes().to_vec();
        data.extend_from_slice(&1_000u64.to_le_bytes());
        data.extend_from_slice(&4u64.to_le_bytes());
        data.extend_from_slice(b"seed");
        data.extend_from_slice(&SYSTEM_PROGRAM_ID.to_bytes());
        let ui = UiCompiledInstruction {
            program_id_index: 0,
            accounts: vec![1, 2, 3],
            data: bs58::encode(&data).into_string(),
            stack_height: None,
        };
        let mut account_keys = vec![SYSTEM_PROGRAM_ID.to_string()];
        for b in 1u8..=4 {
            account_keys.push(key(b));
        }
        let tx = funding_tx_with_meta(&account_keys, vec![ui], None);
        let (from, to) = (
            Pubkey::new_from_array([1; 32]),
            Pubkey::new_from_array([3; 32]),
        );
        assert_eq!(find_funding_source(&tx, &to), Some(from));
        assert_eq!(find_funding_source(&tx, &from), None);
    }

    #[test]
    fn funding_source_found_via_create_with_seed() {
        // CreateAccountWithSeed (tag 3, [from, new, base]).
        let mut data = 3u32.to_le_bytes().to_vec();
        data.extend_from_slice(&Pubkey::new_from_array([2; 32]).to_bytes());
        data.extend_from_slice(&4u64.to_le_bytes());
        data.extend_from_slice(b"seed");
        data.extend_from_slice(&1_000u64.to_le_bytes());
        data.extend_from_slice(&0u64.to_le_bytes());
        data.extend_from_slice(&SYSTEM_PROGRAM_ID.to_bytes());
        let ui = UiCompiledInstruction {
            program_id_index: 0,
            accounts: vec![1, 2, 3],
            data: bs58::encode(&data).into_string(),
            stack_height: None,
        };
        let mut account_keys = vec![SYSTEM_PROGRAM_ID.to_string()];
        for b in 1u8..=4 {
            account_keys.push(key(b));
        }
        let tx = funding_tx_with_meta(&account_keys, vec![ui], None);
        let (from, new) = (
            Pubkey::new_from_array([1; 32]),
            Pubkey::new_from_array([2; 32]),
        );
        assert_eq!(find_funding_source(&tx, &new), Some(from));
    }

    #[test]
    fn funding_source_found_via_ata_creation() {
        // SPL onboarding: the funder pays rent to create the buyer's ATA.
        let payer = Pubkey::new_from_array([1; 32]);
        let ata = Pubkey::new_from_array([2; 32]);
        let owner = Pubkey::new_from_array([3; 32]);
        let mint = Pubkey::new_from_array([4; 32]);
        let order = [
            ASSOCIATED_TOKEN_PROGRAM_ID,
            payer,
            ata,
            owner,
            mint,
            SYSTEM_PROGRAM_ID,
        ];
        for disc in [[0u8], [1u8]] {
            let ui = UiCompiledInstruction {
                program_id_index: 0,
                accounts: vec![1, 2, 3, 4, 5],
                data: bs58::encode(disc).into_string(),
                stack_height: None,
            };
            let tx = funding_tx_with_meta(
                &order.iter().map(|k| k.to_string()).collect::<Vec<_>>(),
                vec![ui],
                None,
            );
            assert_eq!(
                find_funding_source(&tx, &owner),
                Some(payer),
                "disc {disc:?}"
            );
        }
        // ATA created for someone else: not our beneficiary.
        let ui = UiCompiledInstruction {
            program_id_index: 0,
            accounts: vec![1, 2, 3, 4, 5],
            data: bs58::encode([1u8]).into_string(),
            stack_height: None,
        };
        let tx = funding_tx_with_meta(
            &order.iter().map(|k| k.to_string()).collect::<Vec<_>>(),
            vec![ui],
            None,
        );
        assert_eq!(find_funding_source(&tx, &mint), None);
    }

    #[test]
    fn funding_source_found_in_inner_instruction() {
        // CPI-routed funding runs the system transfer as an INNER ix.
        let mut data = 2u32.to_le_bytes().to_vec();
        data.extend_from_slice(&500u64.to_le_bytes());
        let inner = UiCompiledInstruction {
            program_id_index: 0,
            accounts: vec![1, 2],
            data: bs58::encode(data).into_string(),
            stack_height: None,
        };
        let outer = UiCompiledInstruction {
            program_id_index: 3,
            accounts: vec![],
            data: bs58::encode([9u8; 8]).into_string(),
            stack_height: None,
        };
        let other_program = Pubkey::new_from_array([9; 32]);
        let order = [
            SYSTEM_PROGRAM_ID.to_string(),
            key(1),
            key(2),
            other_program.to_string(),
        ];
        let meta = UiTransactionStatusMeta {
            err: None,
            status: Ok(()),
            fee: 5000,
            pre_balances: vec![],
            post_balances: vec![],
            inner_instructions: OptionSerializer::Some(vec![UiInnerInstructions {
                index: 0,
                instructions: vec![UiInstruction::Compiled(inner)],
            }]),
            log_messages: OptionSerializer::None,
            pre_token_balances: OptionSerializer::None,
            post_token_balances: OptionSerializer::None,
            rewards: OptionSerializer::None,
            loaded_addresses: OptionSerializer::None,
            return_data: OptionSerializer::None,
            compute_units_consumed: OptionSerializer::None,
            cost_units: OptionSerializer::None,
        };
        let tx = funding_tx_with_meta(&order, vec![outer], Some(meta));
        assert_eq!(
            find_funding_source(&tx, &Pubkey::new_from_array([2; 32])),
            Some(Pubkey::new_from_array([1; 32]))
        );
    }

    /// Frozen-mainnet v3 replay helper: pins the v3 layout (trader at
    /// index 8, amount = first u64) against real chain fills.
    fn v3_records(raw: &str) -> Vec<SwapRecord> {
        use solana_transaction_status::EncodedConfirmedTransactionWithStatusMeta;
        let fetched: EncodedConfirmedTransactionWithStatusMeta =
            serde_json::from_str(raw).expect("fixture");
        extract_pump_swaps(&fetched, "frozen-v3")
    }

    #[test]
    fn replay_buy_v3_mainnet() {
        let records = v3_records(include_str!("../../tests/fixtures/buy_v3_mainnet.json"));
        // Exactly one real swap; the rest are non-swap pump ixs.
        let buys: Vec<_> = records.iter().filter(|r| r.side == SwapSide::Buy).collect();
        assert_eq!(buys.len(), 1, "exactly one v3 buy");
        assert!(
            records
                .iter()
                .all(|r| r.side == SwapSide::Buy || r.side == SwapSide::Unknown)
        );
        let r = buys[0];
        assert_eq!(
            r.wallet,
            Pubkey::from_str("4zUGAASz4nciz9hKZBiiZVUwRZD1RjtBUKhJ86mJDFkW").expect("wallet")
        );
        assert!(r.amount_base_units > 0, "buy amount must decode");
    }

    #[test]
    fn replay_sell_v3_mainnet() {
        let records = v3_records(include_str!("../../tests/fixtures/sell_v3_mainnet.json"));
        let sells: Vec<_> = records
            .iter()
            .filter(|r| r.side == SwapSide::Sell)
            .collect();
        assert_eq!(sells.len(), 1, "exactly one v3 sell");
        assert!(
            records
                .iter()
                .all(|r| r.side == SwapSide::Sell || r.side == SwapSide::Unknown)
        );
        let r = sells[0];
        assert_eq!(
            r.wallet,
            Pubkey::from_str("4s4vL1TLZSNMxhG6FZgc2GevwLSLP5XyHEhPdAaKqNLR").expect("wallet")
        );
        // Full exit on-chain (pre 30952762059 → post 0): the decoded
        // amount must equal the balance delta to the unit.
        assert_eq!(r.amount_base_units, 30_952_762_059);
    }

    #[test]
    fn replay_legacy_buy_18_mainnet() {
        // Fee-routed 18-account buy carrying the LEGACY buy
        // discriminator (creator/mayhem flow, not canonical v3): the
        // legacy arm already covers it (Buy, trader at 6, first u64) —
        // this pins that tolerance against future arm-tightening.
        // Proven across 3 mainnet samples with the `TradeEvent` user
        // cross-checked on each.
        let records = v3_records(include_str!(
            "../../tests/fixtures/legacy_buy_18_mainnet.json"
        ));
        let buys: Vec<_> = records.iter().filter(|r| r.side == SwapSide::Buy).collect();
        assert_eq!(buys.len(), 1, "exactly one fee-routed buy");
        let r = buys[0];
        assert_eq!(
            r.wallet,
            Pubkey::from_str("ssssswdk4RR8HqkE3uwUWzDbd6mXFTTPjcXBKNzQ57E").expect("wallet")
        );
        assert_eq!(r.amount_base_units, 3_731_003_023_340);
    }

    #[test]
    fn replay_legacy_buy_18_amount_matches_balance_delta() {
        // Independent proof: the buyer token-balance delta equals the
        // classified amount to the unit (same bar as the v2 pin).
        let raw = include_str!("../../tests/fixtures/legacy_buy_18_mainnet.json");
        let fetched: EncodedConfirmedTransactionWithStatusMeta =
            serde_json::from_str(raw).expect("fixture");
        let records = extract_pump_swaps(&fetched, "frozen-18");
        let buy = records
            .iter()
            .find(|r| r.side == SwapSide::Buy)
            .expect("one buy");
        let meta = fetched.transaction.meta.as_ref().expect("fixture meta");
        let buyer = buy.wallet.to_string();
        let delta = |balances: &OptionSerializer<Vec<UiTransactionTokenBalance>>| -> i128 {
            match balances {
                OptionSerializer::Some(list) => list
                    .iter()
                    .filter(|b| matches!(&b.owner, OptionSerializer::Some(o) if o == &buyer))
                    .map(|b| b.ui_token_amount.amount.parse::<i128>().unwrap_or(0))
                    .sum(),
                _ => 0,
            }
        };
        let net = delta(&meta.post_token_balances) - delta(&meta.pre_token_balances);
        assert_eq!(net, buy.amount_base_units as i128);
        assert_eq!(net, 3_731_003_023_340);
    }

    #[test]
    fn replay_buy_exact_quote_in_v3_mainnet() {
        let records = v3_records(include_str!(
            "../../tests/fixtures/buy_exact_quote_in_v3_mainnet.json"
        ));
        let buys: Vec<_> = records.iter().filter(|r| r.side == SwapSide::Buy).collect();
        assert_eq!(buys.len(), 1, "exactly one exact-quote v3 buy");
        assert!(
            records
                .iter()
                .all(|r| r.side == SwapSide::Buy || r.side == SwapSide::Unknown)
        );
        let r = buys[0];
        assert_eq!(
            r.wallet,
            Pubkey::from_str("AMDEmVocLtN5ji2gBY7BrmSHEyM5vt6TGszBbg4crMXg").expect("wallet")
        );
        // Exact-quote carries (spend, min_out): we attribute the
        // conservative `min_tokens_out` (second u64), never the spend.
        assert!(r.amount_base_units > 0, "min_tokens_out must decode");
    }

    #[test]
    fn replay_exact_quote_lifts_to_delivered_delta() {
        // The PostComplete capture is an exact-quote buy with min_out 1
        // that delivered 6,979,344,739,455 base units: forensics must
        // count delivered, not the floor (undercounting biases monopoly
        // screens toward PASS — the unsafe direction).
        let records = v3_records(include_str!(
            "../../tests/fixtures/post_complete_buy_mainnet.json"
        ));
        let buys: Vec<_> = records.iter().filter(|r| r.side == SwapSide::Buy).collect();
        assert_eq!(buys.len(), 1, "exactly one exact-quote buy");
        assert_eq!(buys[0].amount_base_units, 6_979_344_739_455);
    }
}

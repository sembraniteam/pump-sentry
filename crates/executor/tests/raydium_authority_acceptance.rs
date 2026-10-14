//! Raydium authority + bundle acceptance (post-migration exit gate).
//!
//! Two gates, $0 by construction (read-only RPC + simulation with a
//! funded *sampled* payer, sig_verify off — nothing is ever sent):
//! 1. Authority proof: find a direct on-chain `SwapBaseInV2` on the
//!    frozen Fartcoin pool and assert its `accounts[2]` equals the
//!    shared vault authority (`ids::RAYDIUM_VAULT_AUTHORITY`), then
//!    assert both frozen pools' vaults are owned by that same key.
//!    Prints the proving signature. This is what retires hypotheses
//!    about the authority — a per-pool PDA derivation was refuted here
//!    first (derived `CbaWbgUg...` vs on-chain `5Q544fKr...`).
//! 2. Layout simulation: rebuild the ix with OUR builder (fixture slots
//!    + sampled user legs, dust amount, min-out 0) and
//!    `simulateTransaction` it. Unsigned skeleton: proves layout and
//!    the Token-2022 program parameter, not auth.
//!
//! NEVER runs in normal `cargo test` (`#[ignore]`) nor CI. Needs public
//! mainnet RPC (rate limits may require a rerun):
//! `RAYDIUM_ACCEPTANCE=1 cargo test -p executor --test raydium_authority_acceptance -- --ignored --nocapture`

use executor::build_swap_base_in_v2_ix;
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_client::GetConfirmedSignaturesForAddress2Config,
    rpc_config::{RpcSimulateTransactionConfig, RpcTransactionConfig},
};
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;
use solana_transaction::Transaction;
use std::str::FromStr;

const RPC_URL: &str = "https://api.mainnet-beta.solana.com";
const RAYDIUM_PROGRAM: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
/// Proving pool: frozen Fartcoin migration (`raydium_pool_fartcoin.json`).
const FARTCOIN_MINT: &str = "9BB6NFEcjBCtnNLFko2FqVQBq8HHM13kCyYcdQbgpump";

fn decode_b58(s: &str) -> Vec<u8> {
    bs58::decode(s).into_vec().expect("valid base58")
}

/// Shared RPC constructor: `MAINNET_RPC_URL` when set, else public.
/// Mirrors `mainnet_layout_acceptance` — every probe must use this.
fn rpc_client() -> RpcClient {
    let url = std::env::var("MAINNET_RPC_URL")
        .ok()
        .map(|v| v.replace('\\', ""))
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| RPC_URL.to_string());
    RpcClient::new(url)
}

#[tokio::test]
#[ignore]
async fn raydium_authority_and_bundle_simulate() {
    if std::env::var("RAYDIUM_ACCEPTANCE").as_deref() != Ok("1") {
        eprintln!("skipped: set RAYDIUM_ACCEPTANCE=1 to run the Raydium acceptance probe");
        return;
    }
    let rpc = rpc_client();
    // Frozen pool slots (amm/vaults/mints), trust-gated off-chain.
    let raw = include_str!("../../migration/tests/fixtures/raydium_pool_fartcoin.json");
    let v: serde_json::Value = serde_json::from_str(raw).expect("fixture json");
    let parse = |s: &str| Pubkey::from_str(v[s].as_str().expect("pubkey str")).expect("pubkey");
    let (amm, owner) = (parse("amm"), parse("owner"));
    let data: Vec<u8> = v["data"]
        .as_array()
        .expect("data array")
        .iter()
        .map(|b| b.as_u64().expect("byte") as u8)
        .collect();
    let pool = migration::pool::RaydiumV4Pool::parse_validated(
        &amm,
        &data,
        &owner,
        &Pubkey::from_str(FARTCOIN_MINT).expect("mint"),
    )
    .expect("frozen pool validates");
    assert!(pool.is_wsol_based());

    // --- Gate 1: authority proof against a live direct swap. ---
    let raydium = Pubkey::from_str(RAYDIUM_PROGRAM).expect("raydium id");
    assert_eq!(owner, raydium, "fixture owner must be the AMM v4 program");
    let mut before = None;
    let mut proof: Option<(String, Vec<Pubkey>, Pubkey)> = None;
    let mut scanned = 0u32;
    'scan: for _ in 0..4 {
        let sigs: Vec<String> = rpc
            .get_signatures_for_address_with_config(
                &amm,
                GetConfirmedSignaturesForAddress2Config {
                    before,
                    until: None,
                    limit: Some(50),
                    commitment: None,
                },
            )
            .await
            .expect("signatures readable")
            .into_iter()
            .filter(|s| s.err.is_none())
            .map(|s| s.signature)
            .collect();
        if sigs.is_empty() {
            break;
        }
        before = sigs
            .last()
            .and_then(|s| solana_signature::Signature::from_str(s).ok());
        for sig in &sigs {
            scanned += 1;
            let Ok(sig_parsed) = solana_signature::Signature::from_str(sig) else {
                continue;
            };
            let Ok(fetched) = rpc
                .get_transaction_with_config(
                    &sig_parsed,
                    RpcTransactionConfig {
                        encoding: Some(solana_transaction_status::UiTransactionEncoding::Json),
                        commitment: None,
                        max_supported_transaction_version: Some(1),
                    },
                )
                .await
            else {
                continue;
            };
            let solana_transaction_status::EncodedTransaction::Json(ui_tx) =
                &fetched.transaction.transaction
            else {
                continue;
            };
            let solana_transaction_status::UiMessage::Raw(raw) = &ui_tx.message else {
                continue;
            };
            let mut keys: Vec<Pubkey> = raw
                .account_keys
                .iter()
                .filter_map(|k| Pubkey::from_str(k).ok())
                .collect();
            if keys.len() != raw.account_keys.len() {
                continue;
            }
            if let Some(meta) = &fetched.transaction.meta {
                use solana_transaction_status::option_serializer::OptionSerializer;
                if let OptionSerializer::Some(loaded) = &meta.loaded_addresses {
                    for k in loaded.writable.iter().chain(loaded.readonly.iter()) {
                        let Ok(p) = Pubkey::from_str(k) else { continue };
                        keys.push(p);
                    }
                }
            }
            let pump_idx = match keys.iter().position(|k| *k == raydium) {
                Some(i) => i as u8,
                None => continue,
            };
            for cix in &raw.instructions {
                if cix.program_id_index != pump_idx {
                    continue;
                }
                let ix_data = decode_b58(&cix.data);
                // Direct SwapBaseInV2 only (tag 16): router inner ixs
                // carry different account frames and prove nothing here.
                if ix_data.len() != 17 || ix_data[0] != 16 {
                    continue;
                }
                let accounts: Vec<Pubkey> = cix
                    .accounts
                    .iter()
                    .filter_map(|i| keys.get(usize::from(*i)).copied())
                    .collect();
                if accounts.len() != 8 {
                    continue;
                }
                proof = Some((sig.clone(), accounts, keys[0]));
                break 'scan;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    let Some((sig, accounts, payer)) = proof else {
        panic!(
            "no direct SwapBaseInV2 in {scanned} signatures for {amm}; rerun (router txs dominate) — NOT a builder verdict"
        );
    };
    let derived = ids::RAYDIUM_VAULT_AUTHORITY;
    println!("proving tx: {sig}");
    println!("on-chain authority (accounts[2]): {}", accounts[2]);
    println!("vault authority constant:         {derived}");
    assert_eq!(
        accounts[2], derived,
        "authority mismatch on {sig}: do NOT wire"
    );
    println!("AUTHORITY GREEN");
    // Vault-owner cross-proof on BOTH frozen pools: a vault is an SPL
    // token account, so its *program* owner is always a token program —
    // the fund authority lives in data[32..64] (mint 0..32, owner
    // 32..64, amount 64..72). That inner owner must be the shared
    // authority, or swaps built at it fail on-chain. Four reads.
    // NOTE: `include_str!` above is source-relative, but this read is
    // CWD-relative (cargo runs integration tests from the crate root),
    // hence one fewer `../`.
    for (fixture, mint) in [
        (
            "../migration/tests/fixtures/raydium_pool_fartcoin.json",
            FARTCOIN_MINT,
        ),
        (
            "../migration/tests/fixtures/raydium_pool_mainnet.json",
            "HyDKNdnhZNVYQMruBevbNsUWruA9STmAQrS4srXApump",
        ),
    ] {
        let raw = std::fs::read_to_string(fixture).expect("fixture readable");
        let v: serde_json::Value = serde_json::from_str(&raw).expect("fixture json");
        let p = |s: &str| Pubkey::from_str(v[s].as_str().expect("pubkey str")).expect("pubkey");
        let (famm, fowner) = (p("amm"), p("owner"));
        let fdata: Vec<u8> = v["data"]
            .as_array()
            .expect("data array")
            .iter()
            .map(|b| b.as_u64().expect("byte") as u8)
            .collect();
        let fpool = migration::pool::RaydiumV4Pool::parse_validated(
            &famm,
            &fdata,
            &fowner,
            &Pubkey::from_str(mint).expect("mint"),
        )
        .expect("frozen pool validates");
        for vault in [fpool.base_vault, fpool.quote_vault] {
            let data = rpc.get_account_data(&vault).await.expect("vault readable");
            // Raw bytes (client Pubkey version-proof): inner owner must
            // equal the shared authority.
            assert_eq!(
                &data[32..64],
                &ids::RAYDIUM_VAULT_AUTHORITY.to_bytes(),
                "vault {vault} fund authority is not the shared authority on {fixture}"
            );
        }
        println!("vault owners GREEN ({fixture})");
    }

    // --- Gate 2: our builder rebuilds the sampled legs, sim only. ---
    // No token-program resolution: upstream hard-requires classic at
    // [0], so the builder carries no program parameter to resolve.
    let ix = build_swap_base_in_v2_ix(
        &pool.amm_id,
        &derived,
        &pool.base_vault,
        &pool.quote_vault,
        &accounts[5],
        &accounts[6],
        &accounts[7],
        1_000,
        0,
    )
    .expect("replay builds");
    assert_eq!(ix.program_id, raydium);
    let blockhash = rpc.get_latest_blockhash().await.expect("blockhash");
    let ixs: Vec<Instruction> = vec![ix];
    let message = solana_message::Message::new_with_blockhash(&ixs, Some(&payer), &blockhash);
    let tx = Transaction {
        signatures: vec![
            solana_signature::Signature::default();
            message.header.num_required_signatures as usize
        ],
        message,
    };
    let res = rpc
        .simulate_transaction_with_config(
            &tx,
            RpcSimulateTransactionConfig {
                sig_verify: false,
                ..Default::default()
            },
        )
        .await
        .expect("simulate RPC");
    println!(
        "SIM logs:\n{}",
        res.value
            .logs
            .as_ref()
            .map(|l| l.join("\n"))
            .unwrap_or_default()
    );
    assert!(
        res.value.err.is_none(),
        "raydium replay simulation failed: {:?}",
        res.value.err
    );
    println!("RAYDIUM BUNDLE GREEN");

    // --- Gate 3: the SELL direction we will actually dispatch. ---
    // Our stuck bags hold the quote-side token and want WSOL out. Read
    // the sampled user-source mint: WSOL there means the sample was a
    // buy, so flip the legs for our sell (and vice versa).
    let src_data = rpc
        .get_account_data(&accounts[5])
        .await
        .expect("src readable");
    let src_mint = Pubkey::new_from_array(
        src_data[0..32]
            .try_into()
            .expect("token account holds its mint at [0..32]"),
    );
    let (sell_src, sell_dst) = if src_mint == ids::WSOL_MINT {
        (accounts[6], accounts[5])
    } else {
        (accounts[5], accounts[6])
    };
    let sell_ix = build_swap_base_in_v2_ix(
        &pool.amm_id,
        &derived,
        &pool.base_vault,
        &pool.quote_vault,
        &sell_src,
        &sell_dst,
        &accounts[7],
        1_000,
        0,
    )
    .expect("sell replay builds");
    // Fresh blockhash per gate: the scan + vault reads between gates can
    // outlive the previous hash (stale hash proves nothing — resample,
    // like the pump probes).
    let sell_hash = rpc.get_latest_blockhash().await.expect("sell blockhash");
    let sell_msg = solana_message::Message::new_with_blockhash(
        std::slice::from_ref(&sell_ix),
        Some(&payer),
        &sell_hash,
    );
    let sell_tx = Transaction {
        signatures: vec![
            solana_signature::Signature::default();
            sell_msg.header.num_required_signatures as usize
        ],
        message: sell_msg,
    };
    let res = rpc
        .simulate_transaction_with_config(
            &sell_tx,
            RpcSimulateTransactionConfig {
                sig_verify: false,
                ..Default::default()
            },
        )
        .await
        .expect("sell simulate RPC");
    println!(
        "SELL SIM logs:\n{}",
        res.value
            .logs
            .as_ref()
            .map(|l| l.join("\n"))
            .unwrap_or_default()
    );
    assert!(
        res.value.err.is_none(),
        "raydium sell-direction simulation failed: {:?}",
        res.value.err
    );
    println!("RAYDIUM SELL GREEN");
}

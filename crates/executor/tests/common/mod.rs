//! Shared $0 mainnet-probe scaffolding: RPC construction, block scans,
//! fixture parsing, holder discovery, simulation gates.
//!
//! Simulation only, never sends anything. Every helper is fail-open
//! toward *resampling* (empty/false) or fail-loud (panic with logs) —
//! a helper never returns a false GREEN.
//!
//! Dead code is allowed here by design: each integration target
//! compiles its own copy and uses a different subset (raydium needs
//! scans+sims, layout needs fixtures+holders). An unused helper in one
//! target is live code in another — splitting per target would
//! re-create the duplication this module deletes.
#![allow(dead_code)]

use solana_client::{
    nonblocking::rpc_client::RpcClient, rpc_config::RpcSimulateTransactionConfig,
};
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;
use solana_transaction::Transaction;
use std::str::FromStr;

/// Base58 decode or panic (frozen fixtures only — untrusted RPC data
/// must use [`decode_b58_opt`] and skip on `None`, never panic).
pub fn decode_b58(s: &str) -> Vec<u8> {
    bs58::decode(s).into_vec().expect("valid base58")
}

/// Fallible base58 decode for live RPC payloads. A corrupt `data`
/// string from RPC is a skip, never a panic.
pub fn decode_b58_opt(s: &str) -> Option<Vec<u8>> {
    bs58::decode(s).into_vec().ok()
}

/// Shared RPC constructor: `MAINNET_RPC_URL` (Helius, backslash escapes
/// sanitized) when set, else the canonical public endpoint. Every probe
/// must use this — the public node 429s after bulk scans, which turns a
/// 3-minute run into 20 minutes of backoff.
pub fn rpc_client() -> RpcClient {
    let url = std::env::var("MAINNET_RPC_URL")
        .ok()
        .map(|v| v.replace('\\', ""))
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| listener::types::DEFAULT_HTTP_RPC_URL.to_string());
    RpcClient::new(url)
}

/// Simulate a skeleton with `payer` (dust amount, open guards,
/// `sig_verify` off) and assert clean execution. Returns `true` on
/// GREEN; transport trouble returns `false` (caller resamples);
/// layout-level failure panics with the sim logs (the gate that
/// guards funds). Simulation only, never sends anything.
pub async fn sim_skeleton_green(
    rpc: &RpcClient,
    payer: &Pubkey,
    ixs: Vec<Instruction>,
    label: &str,
) -> bool {
    match sim_ix_green(rpc, payer, ixs, label).await {
        Ok(()) => true,
        Err(e) if e.starts_with("transport:") => {
            eprintln!("{label}: {e}; resampling");
            false
        }
        Err(e) => panic!("{label} simulation failed: {e}"),
    }
}

/// Simulate and return the verdict. `Err("transport: ...")` means the
/// RPC/blockhash failed (resample); any other `Err` carries sim logs
/// (layout failure). Fresh blockhash per call: a stale hash proves
/// nothing about layout.
pub async fn sim_ix_green(
    rpc: &RpcClient,
    payer: &Pubkey,
    ixs: Vec<Instruction>,
    label: &str,
) -> Result<(), String> {
    let blockhash = rpc
        .get_latest_blockhash()
        .await
        .map_err(|e| format!("transport: blockhash failed ({e})"))?;
    let message = solana_message::Message::new_with_blockhash(&ixs, Some(payer), &blockhash);
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
        .map_err(|e| format!("transport: simulate RPC failed ({e})"))?;
    let logs = res
        .value
        .logs
        .as_ref()
        .map(|l| l.join("\n"))
        .unwrap_or_default();
    println!("SIM logs ({label}):\n{logs}");
    if let Some(err) = &res.value.err {
        // Stale blockhash under load proves nothing about layout:
        // surface it as transport so the caller resamples.
        let err_str = format!("{err:?}");
        if err_str.contains("BlockhashNotFound") && logs.is_empty() {
            return Err("transport: stale blockhash under load".to_string());
        }
        return Err(format!("{err:?}\n{logs}"));
    }
    println!("{label} GREEN");
    Ok(())
}

/// One decoded instruction: program slot, raw data, account slots.
pub struct ScannedIx {
    pub program_idx: u8,
    pub data: Vec<u8>,
    pub accounts: Vec<usize>,
}

/// One decoded transaction: signature, full key table (static plus
/// address-table-loaded), and instructions.
pub struct ScannedTx {
    pub sig: String,
    pub keys: Vec<Pubkey>,
    pub ixs: Vec<ScannedIx>,
}

/// Fetch one block and decode every successful transaction into
/// account tables + raw instructions. `None` when the slot is
/// unavailable or empty (caller backs off and resamples).
pub async fn scan_block(rpc: &RpcClient, slot: u64) -> Option<Vec<ScannedTx>> {
    let block = rpc
        .get_block_with_config(
            slot,
            solana_client::rpc_config::RpcBlockConfig {
                encoding: Some(solana_transaction_status::UiTransactionEncoding::Json),
                transaction_details: Some(solana_transaction_status::TransactionDetails::Full),
                rewards: Some(false),
                commitment: None,
                max_supported_transaction_version: Some(1),
            },
        )
        .await
        .ok()?;
    let empty = Vec::new();
    let transactions = block.transactions.as_ref().unwrap_or(&empty);
    if transactions.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for tx_meta in transactions {
        // Failed transactions create nothing: their account references
        // (curves, vaults) may never exist on-chain.
        if tx_meta.meta.as_ref().is_some_and(|m| m.err.is_some()) {
            continue;
        }
        let solana_transaction_status::EncodedTransaction::Json(ui_tx) = &tx_meta.transaction
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
        if let Some(meta) = &tx_meta.meta {
            use solana_transaction_status::option_serializer::OptionSerializer;
            if let OptionSerializer::Some(loaded) = &meta.loaded_addresses {
                for k in loaded.writable.iter().chain(loaded.readonly.iter()) {
                    let Ok(p) = Pubkey::from_str(k) else { continue };
                    keys.push(p);
                }
            }
        }
        let ixs = raw
            .instructions
            .iter()
            .filter_map(|cix| {
                Some(ScannedIx {
                    program_idx: cix.program_id_index,
                    data: decode_b58_opt(&cix.data)?,
                    accounts: cix.accounts.iter().map(|i| usize::from(*i)).collect(),
                })
            })
            .collect();
        out.push(ScannedTx {
            sig: ui_tx.signatures.first().cloned().unwrap_or_default(),
            keys,
            ixs,
        });
    }
    Some(out)
}

/// Resolve instruction account slots against the key table.
/// `None` when any slot dangles (pruned/v0 edge, never a verdict).
pub fn resolve_accounts(tx: &ScannedTx, ix: &ScannedIx) -> Option<Vec<Pubkey>> {
    let accounts: Vec<Pubkey> = ix
        .accounts
        .iter()
        .filter_map(|i| tx.keys.get(*i).copied())
        .collect();
    (accounts.len() == ix.accounts.len()).then_some(accounts)
}

/// Read a token account: `(owner, mint, amount)`. `None` when missing
/// or unparsable (closed ATA, short data — resample, never GREEN).
/// Owner/mint come from account data, the amount from the balance
/// call (same two reads the probes always did).
pub async fn token_account(rpc: &RpcClient, ata: &Pubkey) -> Option<(Pubkey, Pubkey, u64)> {
    let data = rpc.get_account_data(ata).await.ok()?;
    if data.len() < 72 {
        return None;
    }
    let owner = Pubkey::new_from_array(data[32..64].try_into().ok()?);
    let mint = Pubkey::new_from_array(data[0..32].try_into().ok()?);
    let amount: u64 = rpc
        .get_token_account_balance(ata)
        .await
        .ok()?
        .amount
        .parse()
        .ok()?;
    Some((owner, mint, amount))
}

/// Canonical holders of `mint`: `(owners, aux_dropped)`, where owners
/// is `(owner, ata, base amount)`, curve vault excluded, aux
/// (non-canonical) ATAs dropped and counted. Retries the heavy
/// indexing call with backoff (public RPC 429s it first); empty after
/// retries means "no data", never "no holders".
pub async fn holder_atas(
    rpc: &RpcClient,
    mint: &Pubkey,
    curve: &Pubkey,
) -> (Vec<(Pubkey, Pubkey, u64)>, u64) {
    let token_program = match rpc.get_account(mint).await {
        Ok(a) => a.owner,
        Err(_) => return (Vec::new(), 0),
    };
    let mut holders = None;
    for attempt in 0..3u32 {
        match tokio::time::timeout(
            std::time::Duration::from_secs(15),
            rpc.get_token_largest_accounts(mint),
        )
        .await
        {
            Ok(Ok(h)) => {
                holders = Some(h);
                break;
            }
            _ if attempt < 2 => {
                eprintln!("largest-accounts unreadable for {mint} (attempt {}/3; backing off)", attempt + 1);
                tokio::time::sleep(std::time::Duration::from_secs(2 * (attempt as u64 + 1))).await;
            }
            _ => {
                eprintln!("largest-accounts unreadable for {mint} (throttled after 3 attempts)");
            }
        }
    }
    let mut out = Vec::new();
    let mut aux = 0u64;
    for h in holders.unwrap_or_default().iter().take(10) {
        let Ok(hata) = Pubkey::from_str(&h.address) else { continue };
        let (owner, _, base) = match token_account(rpc, &hata).await {
            Some(v) => v,
            None => continue,
        };
        if owner == *curve {
            continue;
        }
        if executor::instructions::user_ata_with_program(&owner, mint, &token_program) != hata {
            aux += 1;
            continue;
        }
        out.push((owner, hata, base));
    }
    (out, aux)
}

/// Fixture v3 instruction accounts in 17-shape order. Positions:
/// `[1]` mint, `[2]` quote, `[5]` curve, `[6]` assoc, `[8]`
/// payer/trader, `[9]` base ATA, `[13]` buyback. Panics when the
/// fixture carries no v3 ix (frozen set, never user input).
fn fixture_v3_slots(raw: &str, path: &str) -> Vec<Pubkey> {
    let v: serde_json::Value = serde_json::from_str(raw).expect("fixture json");
    let msg = &v["transaction"]["message"];
    // String table (unfiltered): slot positions must not shift.
    let mut keys: Vec<String> =
        serde_json::from_value(msg["accountKeys"].clone()).expect("keys");
    if let Some(loaded) = v["meta"]["loadedAddresses"].as_object() {
        for table in ["writable", "readonly"] {
            for addr in loaded
                .get(table)
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
            {
                keys.push(addr.as_str().expect("addr").to_string());
            }
        }
    }
    let pump = ids::PUMP_PROGRAM_ID.to_string();
    let pi = keys.iter().position(|k| k == &pump).expect("pump key");
    for ix in msg["instructions"].as_array().expect("ixs") {
        if ix["programIdIndex"].as_u64() != Some(pi as u64) {
            continue;
        }
        let data =
            bs58::decode(ix["data"].as_str().expect("data")).into_vec().expect("b58");
        let is_v3 = (data.starts_with(&ids::PUMP_BUY_V3_DISCRIMINATOR)
            || data.starts_with(&ids::PUMP_SELL_V3_DISCRIMINATOR))
            && data.len() == 24;
        if !is_v3 {
            continue;
        }
        let idx: Vec<usize> = serde_json::from_value(ix["accounts"].clone()).expect("accts");
        assert_eq!(idx.len(), 17, "fixture {path} must be 17 accounts");
        return idx
            .into_iter()
            .map(|i| Pubkey::from_str(&keys[i]).expect("pubkey"))
            .collect();
    }
    panic!("no v3 ix in fixture {path}");
}

/// Fixture's own trader: `[8]` payer and `[9]` base ATA (the offline
/// fallback when largest-holders is throttled).
pub fn fixture_v3_trader(raw: &str, path: &str) -> (Pubkey, Pubkey) {
    let a = fixture_v3_slots(raw, path);
    (a[8], a[9])
}

/// Fixture v3 layout: `(mint, quote, curve, assoc, buyback)`.
pub fn fixture_v3_accounts(raw: &str, path: &str) -> (Pubkey, Pubkey, Pubkey, Pubkey, Pubkey) {
    let a = fixture_v3_slots(raw, path);
    (a[1], a[2], a[5], a[6], a[13])
}

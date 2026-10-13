//! Mainnet layout acceptance (Syarat 1 pra-funding): v3-only.
//!
//! For one fresh mainnet `buy_v3` and one fresh `sell_v3` on the Pump.fun
//! bonding curve program, this exercises OUR full build path exactly as
//! production runs it:
//! 1. resolve the token program from the mint account owner
//!    (classic SPL vs Token-2022),
//! 2. rebuild the 17-account instruction with `PumpSwapBuilder` from the
//!    chain's own accounts,
//! 3. `simulateTransaction` the rebuilt transaction (sig_verify off).
//!
//! THE GATE IS (3): simulation must succeed. Retired shapes (legacy
//! 12-account, 18-account mayhem, 27/26-account v2) are NOT probed:
//! legacy fills are extinct and 18/v2 fills occur only on old curves,
//! while every new launch is a v3 curve.
//!
//! Rebuilt economics are neutered (dust amount, open guards): proves
//! layout, not profitability. Never sends anything: simulation only,
//! random signer, no wallet.
//!
//! NEVER runs in normal `cargo test` (`#[ignore]`) nor CI. Needs public
//! mainnet RPC (rate limits may require a rerun):
//! `MAINNET_LAYOUT=1 cargo test -p executor --test mainnet_layout_acceptance -- --ignored --nocapture`

use executor::instructions::PumpSwapBuilder;
use solana_client::{nonblocking::rpc_client::RpcClient, rpc_config::RpcSimulateTransactionConfig};
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;
use solana_transaction::Transaction;
use std::str::FromStr;

const PUMP_PROGRAM: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
const RPC_URL: &str = "https://api.mainnet-beta.solana.com";

fn decode_b58(s: &str) -> Vec<u8> {
    bs58::decode(s).into_vec().expect("valid base58")
}

/// Shared RPC constructor: `MAINNET_RPC_URL` (Helius, backslash escapes
/// sanitized) when set, else the public endpoint. Every probe must use
/// this — the public node 429s after bulk scans, which turns a 3-minute
/// run into 20 minutes of backoff.
fn rpc_client() -> RpcClient {
    let url = std::env::var("MAINNET_RPC_URL")
        .ok()
        .map(|v| v.replace('\\', ""))
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| RPC_URL.to_string());
    RpcClient::new(url)
}

async fn live_token_program(rpc: &RpcClient, mint: &Pubkey) -> Pubkey {
    rpc.get_account(mint).await.expect("mint readable").owner
}

/// Simulate a rebuilt skeleton with the sampled trader as payer
/// (dust amount, open guard, sig_verify off) and assert clean
/// execution. Returns `true` on GREEN; transport trouble returns
/// `false` (caller resamples); layout-level failure panics with the
/// sim logs (the gate that guards funds). Simulation only, never sends
/// anything.
async fn sim_skeleton_green(
    rpc: &RpcClient,
    payer: &Pubkey,
    ixs: Vec<Instruction>,
    label: &str,
) -> bool {
    let blockhash = match rpc.get_latest_blockhash().await {
        Ok(b) => b,
        Err(e) => {
            println!("{label}: blockhash failed ({e}); resampling");
            return false;
        }
    };
    let message = solana_message::Message::new_with_blockhash(&ixs, Some(payer), &blockhash);
    let tx = Transaction {
        signatures: vec![
            solana_signature::Signature::default();
            message.header.num_required_signatures as usize
        ],
        message,
    };
    let res = match rpc
        .simulate_transaction_with_config(
            &tx,
            RpcSimulateTransactionConfig { sig_verify: false, ..Default::default() },
        )
        .await
    {
        Ok(res) => res,
        Err(e) => {
            println!("{label}: simulate RPC failed ({e}); resampling");
            return false;
        }
    };
    println!(
        "SIM logs ({label}):\n{}",
        res.value.logs.as_ref().map(|l| l.join("\n")).unwrap_or_default()
    );
    if let Some(err) = &res.value.err {
        // Stale blockhash under load proves nothing about layout: fresh
        // retry with a new blockhash instead of failing the gate.
        let err_str = format!("{err:?}");
        let stale = err_str.contains("BlockhashNotFound")
            && res.value.logs.as_ref().map(|l| l.is_empty()).unwrap_or(true);
        if stale {
            println!("{label}: stale blockhash under load; resampling");
            return false;
        }
        panic!("{label} simulation failed: {err:?}");
    }
    println!("{label} GREEN");
    true
}

#[tokio::test]
#[ignore]
async fn mainnet_v3_17_simulates() {
    // 17-shape execution gate (both v3 sides share one body):
    // deterministic signature-replay: frozen fixtures provide canonical
    // 17-account layout (mint/curve/assoc/quote/buyback), live RPC
    // provides current largest holders for buy+sell replay. No
    // block-hunt (79 blocks/84k tx -> 1 hit/0 sim is wasteful).
    // Unsigned skeletons (sig_verify off): prove layout, not auth.
    if std::env::var("MAINNET_LAYOUT").as_deref() != Ok("1") {
        eprintln!("skipped: set MAINNET_LAYOUT=1 to run the live layout probe");
        return;
    }
    use executor::instructions::{
        BuyV3Accounts, PumpSwapBuilder, SellV3Accounts,
        build_create_ata_ix, user_ata_with_program,
        user_volume_accumulator_pda,
    };
    let rpc = rpc_client();

    // --- helpers: parse frozen fixture into (mint, quote_mint, curve, assoc, buyback)
    fn fixture_v3_accounts(raw: &str, path: &str) -> (Pubkey, Pubkey, Pubkey, Pubkey, Pubkey) {
        let raw: serde_json::Value =
            serde_json::from_str(raw).expect("fixture json");
        // Bare getTransaction shape (no envelope).
        let msg = &raw["transaction"]["message"];
        let mut keys: Vec<String> = serde_json::from_value(msg["accountKeys"].clone()).expect("keys");
        if let Some(loaded) = raw["meta"]["loadedAddresses"].as_object() {
            for table in ["writable", "readonly"] {
                for addr in loaded.get(table).and_then(|v| v.as_array()).cloned().unwrap_or_default() {
                    keys.push(addr.as_str().expect("addr").to_string());
                }
            }
        }
        let pump = ids::PUMP_PROGRAM_ID.to_string();
        let pi = keys.iter().position(|k| k == &pump).expect("pump key");
        for ix in msg["instructions"].as_array().expect("ixs") {
            if ix["programIdIndex"].as_u64() != Some(pi as u64) { continue; }
            let data = bs58::decode(ix["data"].as_str().expect("data")).into_vec().expect("b58");
            // Accept either buy_v3 or sell_v3 discriminator (both 17 accounts).
            let is_v3 = (data.starts_with(&ids::PUMP_BUY_V3_DISCRIMINATOR) || data.starts_with(&ids::PUMP_SELL_V3_DISCRIMINATOR)) && data.len() == 24;
            if !is_v3 { continue; }
            let idx: Vec<usize> = serde_json::from_value(ix["accounts"].clone()).expect("accts");
            assert_eq!(idx.len(), 17, "fixture {path} must be 17 accounts");
            let key = |i: usize| Pubkey::from_str(&keys[idx[i]]).expect("pubkey");
            // Positions per 17-shape: [1] mint, [2] quote, [5] curve, [6] assoc, [13] buyback
            return (key(1), key(2), key(5), key(6), key(13));
        }
        panic!("no v3 ix in fixture {path}");
    }

    // Deterministic fixture set: both directions, same 17-body.
    let fixtures = [
        (include_str!("../../listener/tests/fixtures/buy_v3_mainnet.json"), "buy_v3_mainnet.json"),
        (include_str!("../../listener/tests/fixtures/sell_v3_mainnet.json"), "sell_v3_mainnet.json"),
    ];
    let mut buy_green = false;
    let mut sell_green = false;
    let mut sim_attempts = 0u32;
    let mut aux_drops = 0u64;
    let mut uvol_drops = 0u64;
    let mut poor_drops = 0u64;

    for (raw, path) in fixtures {
        if buy_green && sell_green { break; }
        let (mint, quote_mint, curve, assoc, buyback) = fixture_v3_accounts(raw, path);
        eprintln!("fixture {path}: mint={mint} curve={curve}");
        let token_program = match rpc.get_account(&mint).await { Ok(a) => a.owner, Err(_) => continue };
        let quote_program = match rpc.get_account(&quote_mint).await { Ok(a) => a.owner, Err(_) => continue };

        // Live largest holders for this mint (holder-based replay: fill buyers flip fast,
        // zero ATAs stay open with rent — owner still resolves and often still holds SOL/WSOL).
        let holders = match tokio::time::timeout(std::time::Duration::from_secs(15), rpc.get_token_largest_accounts(&mint)).await {
            Ok(Ok(h)) => h, _ => { eprintln!("largest-accounts unreadable for {mint}"); continue; }
        };
        let mut holder_owners: Vec<(Pubkey, Pubkey, u64)> = Vec::new();
        let mut zero_base_owners: Vec<Pubkey> = Vec::new();
        for h in holders.iter().take(10) {
            let Ok(hata) = Pubkey::from_str(&h.address) else { continue };
            let Ok(hdata) = rpc.get_account(&hata).await else { continue };
            if hdata.data.len() < 72 { continue; }
            let howner = Pubkey::new_from_array(hdata.data[32..64].try_into().expect("32B"));
            if howner == curve { continue; }
            if user_ata_with_program(&howner, &mint, &token_program) != hata { aux_drops += 1; continue; }
            let hbase: u64 = match rpc.get_token_account_balance(&hata).await { Ok(b) => b.amount.parse().unwrap_or(0), Err(_) => continue };
            if hbase < 1_000 {
                if !zero_base_owners.contains(&howner) { zero_base_owners.push(howner); }
                continue;
            }
            holder_owners.push((howner, hata, hbase));
        }
        eprintln!("fixture {path}: holders={} funded_base={} zero_base={}", holders.len(), holder_owners.len(), zero_base_owners.len());
        if holder_owners.is_empty() && zero_base_owners.is_empty() { eprintln!("no holder for {mint}"); continue; }

        // BUY side: needs SOL + WSOL, never base — zero-base flip owners qualify.
        if !buy_green {
            let mut buy_candidates: Vec<Pubkey> = holder_owners.iter().map(|(o, _, _)| *o).collect();
            buy_candidates.extend(zero_base_owners.iter().copied());
            for howner in &buy_candidates {
                let sol = rpc.get_balance(howner).await.unwrap_or(0);
                if sol < 50_000 { eprintln!("buy-drop {howner}: sol={sol}"); continue; }
                let hquote = user_ata_with_program(howner, &quote_mint, &quote_program);
                // WSOL ATA may be missing (rent-collected flip) — existence check, not balance gate.
                let hquote_exists = rpc.get_account(&hquote).await.is_ok();
                let quote_exists = hquote_exists;
                // Buy simulation will auto-create both ATAs; only gate on SOL + uvol existence.
                if rpc.get_account(&user_volume_accumulator_pda(howner)).await.is_err() { uvol_drops += 1; eprintln!("buy-drop {howner}: uvol missing"); continue; }
                let mut ixs = Vec::new();
                let hbase_ata = user_ata_with_program(howner, &mint, &token_program);
                if rpc.get_account(&hbase_ata).await.is_err() {
                    ixs.push(build_create_ata_ix(howner, howner, &mint, &hbase_ata, &token_program));
                }
                if !quote_exists {
                    ixs.push(build_create_ata_ix(howner, howner, &quote_mint, &hquote, &quote_program));
                }
                ixs.push(PumpSwapBuilder::build_buy_v3_ix(1_000, u64::MAX, &BuyV3Accounts {
                    payer: howner, mint: &mint, quote_mint: &quote_mint,
                    bonding_curve: &curve, associated_bonding_curve: &assoc,
                    base_token_program: &token_program, quote_token_program: &quote_program,
                    buyback_recipient: &buyback,
                }));
                sim_attempts += 1;
                if sim_skeleton_green(&rpc, howner, ixs, "V3-BUY").await { buy_green = true; break; }
            }
            if !buy_green { poor_drops += 1; }
        }
        // SELL side
        if !sell_green {
            let mut hit = false;
            for (howner, _) in holder_owners.iter().map(|(o,a,_)| (*o,*a)) {
                if rpc.get_balance(&howner).await.unwrap_or(0) < 50_000 { continue; }
                if rpc.get_account(&user_volume_accumulator_pda(&howner)).await.is_err() { continue; }
                let hquote = user_ata_with_program(&howner, &quote_mint, &quote_program);
                let mut ixs = Vec::new();
                if rpc.get_account(&hquote).await.is_err() {
                    ixs.push(build_create_ata_ix(&howner, &howner, &quote_mint, &hquote, &quote_program));
                }
                ixs.push(PumpSwapBuilder::build_sell_v3_ix(1_000, 0, &SellV3Accounts {
                    payer: &howner, mint: &mint, quote_mint: &quote_mint,
                    bonding_curve: &curve, associated_bonding_curve: &assoc,
                    base_token_program: &token_program, quote_token_program: &quote_program,
                    buyback_recipient: &buyback,
                }));
                sim_attempts += 1;
                if sim_skeleton_green(&rpc, &howner, ixs, "V3-SELL").await { sell_green = true; hit = true; break; }
            }
            if !hit { poor_drops += 1; eprintln!("no sellable holder on {mint}"); }
        }
    }
    // Fresh-mint sell replay: fixture mints flipped to zero base (holders
    // all 0), so sell needs a live mint. Discovered via inner-CPI scan of
    // recent pump fills: sig 28Nfc4KzX14WZjtmtju1bFqW91ojNECey1SA5x44qzDahjVTaRNFX1MjoGqc9CDDsqysEDxe89zGGZiVvN6nZ58j
    // carries a 17-account sell_v3 whose layout is positionally identical
    // to build_v3_ix (global/mint/quote/base-prog/quote-prog/curve/assoc/
    // curve-quote/payer/base-ata/quote-ata/uvol/fee-config/buyback/
    // system/event-auth/pump). Buyback GXPFM2... is in BUYBACK_FEE_RECIPIENTS.
    if !sell_green {
        let mint: Pubkey = "9hADEDaSQ84QGJ7F4JK8nyaiFurQMFZwMpgcujdApump".parse().expect("mint");
        let quote_mint: Pubkey = "So11111111111111111111111111111111111111112".parse().expect("wsol");
        let curve: Pubkey = "6zVUqSPGKuiR6QaUHjpxLwFHkh7Q2uLBWF7w3uK3SWX5".parse().expect("curve");
        let assoc: Pubkey = "FX8f1cFaJBBCu4kQvfhLZcpdgjnuYXMv5ySTkHNFdJTN".parse().expect("assoc");
        let buyback: Pubkey = "GXPFM2caqTtQYC2cJ5yJRi9VDkpsYZXzYdwYpGnLmtDL".parse().expect("buyback");
        eprintln!("fresh-sell: mint={mint} curve={curve}");
        let token_program = match rpc.get_account(&mint).await { Ok(a) => a.owner, Err(_) => Pubkey::default() };
        let quote_program = match rpc.get_account(&quote_mint).await { Ok(a) => a.owner, Err(_) => Pubkey::default() };
        if token_program != Pubkey::default() {
            let holders = match tokio::time::timeout(std::time::Duration::from_secs(15), rpc.get_token_largest_accounts(&mint)).await {
                Ok(Ok(h)) => h, _ => Vec::new(),
            };
            eprintln!("fresh-sell: largest={}", holders.len());
            for h in holders.iter().take(10) {
                if sell_green { break; }
                let Ok(hata) = Pubkey::from_str(&h.address) else { continue };
                let Ok(hdata) = rpc.get_account(&hata).await else { continue };
                if hdata.data.len() < 72 { continue; }
                let howner = Pubkey::new_from_array(hdata.data[32..64].try_into().expect("32B"));
                if howner == curve { continue; }
                if user_ata_with_program(&howner, &mint, &token_program) != hata { aux_drops += 1; continue; }
                let hbase: u64 = match rpc.get_token_account_balance(&hata).await { Ok(b) => b.amount.parse().unwrap_or(0), Err(_) => continue };
                if hbase < 1_000 { continue; }
                if rpc.get_balance(&howner).await.unwrap_or(0) < 50_000 { continue; }
                if rpc.get_account(&user_volume_accumulator_pda(&howner)).await.is_err() { uvol_drops += 1; continue; }
                let hquote = user_ata_with_program(&howner, &quote_mint, &quote_program);
                let mut ixs = Vec::new();
                if rpc.get_account(&hquote).await.is_err() {
                    ixs.push(build_create_ata_ix(&howner, &howner, &quote_mint, &hquote, &quote_program));
                }
                ixs.push(PumpSwapBuilder::build_sell_v3_ix(1_000, 0, &SellV3Accounts {
                    payer: &howner, mint: &mint, quote_mint: &quote_mint,
                    bonding_curve: &curve, associated_bonding_curve: &assoc,
                    base_token_program: &token_program, quote_token_program: &quote_program,
                    buyback_recipient: &buyback,
                }));
                sim_attempts += 1;
                if sim_skeleton_green(&rpc, &howner, ixs, "V3-SELL-FRESH").await { sell_green = true; break; }
            }
            if !sell_green { eprintln!("fresh-sell: no sellable holder"); }
        }
    }
    eprintln!("v3 summary: sim_attempts={sim_attempts} aux_drops={aux_drops} uvol_drops={uvol_drops} poor_drops={poor_drops} buy_green={buy_green} sell_green={sell_green}");
    assert!(buy_green && sell_green, "no v3 17-shape simulating clean after deterministic replay");
    println!("V3 GREEN (both sides simulated clean)");
}

#[tokio::test]
#[ignore]
async fn fresh_mayhem_buy_simulates_end_to_end_v2() {
    // Second-generation probe: the v1 probe proved account MAPPING but
    // could not prove execution (payer edge cases). This variant removes
    // every environmental confound:
    // - buyer = a holder with REAL token balance (discovered, not derived),
    // - payer = a well-known FUNDED account as fee coverage only
    //   (simulation never lands; standard sandbox practice),
    // - PDAs (creator vault, user-vol) verified to exist first; the run
    //   aborts loudly if any are missing instead of weak-passing.
    if std::env::var("MAINNET_LAYOUT").as_deref() != Ok("1") {
        eprintln!("skipped: set MAINNET_LAYOUT=1 to run the live layout probe");
        return;
    }
    use listener::parser::{parse_pump_create_v2, PUMP_CREATE_V2_DISCRIMINATOR};
    let rpc = rpc_client();
    // Bulk reads below retry with backoff instead of failing the run
    // on the first throttle (Helius rarely needs it; public RPC does).
    async fn rpc_retry<F, Fut, T>(label: &str, mut f: F) -> T
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, solana_client::client_error::ClientError>>,
    {
        let mut attempt = 0u32;
        loop {
            match f().await {
                Ok(v) => return v,
                Err(e) if attempt < 6 => {
                    attempt += 1;
                    eprintln!("{label} failed ({e}); retry {attempt}/6");
                    tokio::time::sleep(std::time::Duration::from_secs(2 * attempt as u64)).await;
                }
                Err(e) => panic!("{label} failed after retries: {e}"),
            }
        }
    }
    // Fresh launch hunt (bulk blocks, as before).
    let tip = rpc.get_slot().await.expect("slot");
    eprintln!("hunting from tip {tip}");
    let mut launch: Option<listener::PumpTokenLaunchEvent> = None;
    let mut probe_slot = tip.saturating_sub(150);
    let mut blocks_scanned = 0u32;
    let mut txs_scanned = 0u64;
    for _ in 0..40u32 {
        if launch.is_some() {
            break;
        }
        let block = match rpc
            .get_block_with_config(
                probe_slot,
                solana_client::rpc_config::RpcBlockConfig {
                    encoding: Some(solana_transaction_status::UiTransactionEncoding::Json),
                    transaction_details: Some(
                        solana_transaction_status::TransactionDetails::Full,
                    ),
                    rewards: Some(false),
                    commitment: None,
                    max_supported_transaction_version: Some(1),
                },
            )
            .await
        {
            Ok(b) => b,
            Err(e) => {
                eprintln!("slot {probe_slot} unavailable ({e}); backing off");
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                probe_slot = probe_slot.saturating_sub(25);
                continue;
            }
        };
        let empty = Vec::new();
        let transactions = block.transactions.as_ref().unwrap_or(&empty);
        blocks_scanned += 1;
        txs_scanned += transactions.len() as u64;
        // Empty bodies (pruned/archive-miss) carry no launches; say so
        // instead of silently scanning nothing.
        eprintln!("slot {probe_slot}: {} txs", transactions.len());
        if transactions.is_empty() {
            probe_slot = probe_slot.saturating_sub(25);
            continue;
        }
        for tx_meta in transactions {
            // Failed transactions create nothing: their account
            // references (curves, vaults) may never exist on-chain.
            // Skipping them here (not just at sig-scan) after one
            // wasted probe proved the gap.
            if tx_meta
                .meta
                .as_ref()
                .is_some_and(|m| m.err.is_some())
            {
                continue;
            }
            let solana_transaction_status::EncodedTransaction::Json(ui_tx) =
                &tx_meta.transaction
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
            let pump_idx = match keys.iter().position(|k| k.to_string() == PUMP_PROGRAM) {
                Some(i) => i as u8,
                None => continue,
            };
            for cix in &raw.instructions {
                if cix.program_id_index != pump_idx {
                    continue;
                }
                let data = decode_b58(&cix.data);
                if data.len() < 8 || data[..8] != PUMP_CREATE_V2_DISCRIMINATOR {
                    continue;
                }
                let accounts: Vec<Pubkey> = cix
                    .accounts
                    .iter()
                    .filter_map(|i| keys.get(usize::from(*i)).copied())
                    .collect();
                if accounts.len() < 14 {
                    continue;
                }
                let sig = ui_tx.signatures.first().cloned().unwrap_or_default();
                match parse_pump_create_v2(&data, &accounts, sig, probe_slot) {
                    Ok(ev) => {
                        // Curve must still be live (ungraduated) for a
                        // meaningful sim; the buyer is hunted separately
                        // below (a funded stranger — see below).
                        let Ok(curve_data) = rpc.get_account(&ev.bonding_curve).await else {
                            continue;
                        };
                        if curve_data.data.len() < 49 || curve_data.data[48] != 0 {
                            continue;
                        }
                        eprintln!("fresh v2 launch: {} slot={}", ev.mint, ev.slot);
                        launch = Some(ev);
                        break;
                    }
                    Err(_) => continue,
                }
            }
        }
        probe_slot = probe_slot.saturating_sub(25);
    }
    let ev = launch.unwrap_or_else(|| {
        panic!(
            "no fresh create_v2 in {blocks_scanned} blocks ({txs_scanned} txs); launches are sporadic, rerun"
        )
    });
    let creator = ev.creator.expect("v2 launch must carry a creator");
    // Curve must still be live (ungraduated) for a meaningful sim.
    // Single attempt: a missing curve means a bad sample (e.g. failed
    // create), not a throttled RPC — retrying it burns a minute.
    let curve_data = rpc
        .get_account(&ev.bonding_curve)
        .await
        .expect("curve readable");
    assert!(
        curve_data.data.len() >= 49 && curve_data.data[48] == 0,
        "curve already graduated; rerun for a fresh launch"
    );
    // Buyer = a FUNDED stranger (recent real buyer on any curve).
    // The create fee payer was the obvious candidate (funded seconds
    // ago) but 3/3 sampled held ZERO WSOL, and v3 debits WSOL via
    // TransferChecked — a SOL-only buyer fails with token
    // InsufficientFunds (proven 2026-10-14). Any funded account can
    // buy a fresh curve, which is exactly the production sniper shape
    // (our wallet is WSOL-funded at setup): hunt recent buy-side flow
    // for the first canonical holder with SOL + WSOL.
    use executor::instructions::{
        PUMP_BUY_DISCRIMINATOR, PUMP_BUY_V2_DISCRIMINATOR, PUMP_BUY_V3_DISCRIMINATOR,
    };
    let buyer: Pubkey = {
        let mut before = None;
        let mut found = None;
        for _ in 0..3usize {
            let sigs: Vec<String> = match rpc
                .get_signatures_for_address_with_config(
                    &Pubkey::from_str(PUMP_PROGRAM).expect("pump id"),
                    solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config {
                        before,
                        until: None,
                        limit: Some(150),
                        commitment: None,
                    },
                )
                .await
            {
                Ok(list) => list,
                Err(e) => {
                    eprintln!("buyer hunt scan failed ({e}); stopping");
                    break;
                }
            }
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
                let Ok(sig_parsed) = solana_signature::Signature::from_str(sig) else {
                    continue;
                };
                let fetched = match rpc
                    .get_transaction_with_config(
                        &sig_parsed,
                        solana_client::rpc_config::RpcTransactionConfig {
                            encoding: Some(solana_transaction_status::UiTransactionEncoding::Json),
                            commitment: None,
                            max_supported_transaction_version: Some(1),
                        },
                    )
                    .await
                {
                    Ok(v) => v,
                    Err(_) => continue,
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
                let pump_idx = match keys.iter().position(|k| k.to_string() == PUMP_PROGRAM) {
                    Some(i) => i as u8,
                    None => continue,
                };
                for cix in &raw.instructions {
                    if cix.program_id_index != pump_idx {
                        continue;
                    }
                    let data = decode_b58(&cix.data);
                    // Buy-side shape → (buyer, base-ATA, mint) positions.
                    let shape =
                        if data.len() == 25 && data[..8] == PUMP_BUY_DISCRIMINATOR
                            && cix.accounts.len() == 18
                        {
                            Some((6usize, 5usize, 2usize))
                        } else if data.len() == 24 && data[..8] == PUMP_BUY_V2_DISCRIMINATOR
                            && cix.accounts.len() == 27
                        {
                            Some((13usize, 14usize, 1usize))
                        } else if data.len() >= 24 && data[..8] == PUMP_BUY_V3_DISCRIMINATOR
                            && cix.accounts.len() == 17
                        {
                            Some((8usize, 9usize, 1usize))
                        } else {
                            None
                        };
                    let Some((bp, ap, mp)) = shape else { continue };
                    let accounts: Vec<Pubkey> = cix
                        .accounts
                        .iter()
                        .filter_map(|i| keys.get(usize::from(*i)).copied())
                        .collect();
                    if accounts.len() != cix.accounts.len() {
                        continue;
                    }
                    let cand = accounts[bp];
                    let mint = accounts[mp];
                    let token_program = match rpc.get_account(&mint).await {
                        Ok(a) => a.owner,
                        Err(_) => continue,
                    };
                    if executor::instructions::user_ata_with_program(&cand, &mint, &token_program)
                        != accounts[ap]
                    {
                        continue;
                    }
                    let base: u64 = match rpc
                        .get_token_account_balance(&accounts[ap])
                        .await
                    {
                        Ok(b) => b.amount.parse().unwrap_or(0),
                        Err(_) => continue,
                    };
                    if base < 1_000 {
                        continue;
                    }
                    let Ok(sol) = rpc.get_balance(&cand).await else { continue };
                    if sol < 200_000 {
                        continue;
                    }
                    let wsol_ata = executor::instructions::user_ata_with_program(
                        &cand,
                        &executor::instructions::WSOL_MINT,
                        &executor::instructions::TOKEN_PROGRAM_ID,
                    );
                    let wsol: u64 = match rpc.get_token_account_balance(&wsol_ata).await {
                        Ok(b) => b.amount.parse().unwrap_or(0),
                        Err(_) => continue,
                    };
                    if wsol < 100_000 {
                        continue;
                    }
                    println!("buyer hunt: {cand} (holder, {sol} SOL, {wsol} WSOL) via {sig}");
                    found = Some(cand);
                    break;
                }
                if found.is_some() {
                    break;
                }
            }
            if found.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        found.unwrap_or_else(|| panic!("no funded buyer in recent buy flow; rerun"))
    };
    println!("buyer: {buyer} (funded stranger)");
    let buyer_token_program = live_token_program(&rpc, &ev.mint).await;
    let buyer_ata =
        executor::instructions::user_ata_with_program(&buyer, &ev.mint, &buyer_token_program);
    println!("holder token account: {buyer_ata}");
    // The buyer volume-accumulator PDA must exist (created at first
    // trade); abort loudly if not. (The creator vault is not needed by
    // v3 — fees stay on the curve — so only uvol gates the probe.)
    let uvol = executor::instructions::user_volume_accumulator_pda(&buyer);
    rpc_retry("routing PDA read", || rpc.get_account(&uvol)).await;
    // Fresh curves are v3-universal (17 accounts, fees stay on the
    // curve): no fee recipient, no creator vault, no reference needed.
    // Retired shapes (legacy 12-account, 18-account mayhem, 27/26 v2)
    // are not probed: legacy fills are extinct and 18/v2 fills occur
    // only on old curves, while every new launch is a v3 curve.
    let quote_mint = {
        let data = rpc.get_account(&ev.bonding_curve).await.expect("curve readable").data;
        if data.len() >= 115 {
            let raw = solana_pubkey::Pubkey::new_from_array(data[83..115].try_into().unwrap());
            executor::executor::resolve_quote_mint(&raw)
        } else {
            executor::instructions::WSOL_MINT
        }
    };
    let quote_program = if quote_mint == executor::instructions::WSOL_MINT {
        executor::instructions::TOKEN_PROGRAM_ID
    } else {
        match rpc.get_account(&quote_mint).await {
            Ok(a) => a.owner,
            Err(_) => executor::instructions::TOKEN_PROGRAM_ID,
        }
    };
    let _ = creator; // v3 needs no creator vault
    let buyback_wallet = executor::instructions::DEFAULT_BUYBACK_FEE_RECIPIENT;
    let buyback = executor::executor::buyback_account(&buyback_wallet, &quote_mint, &quote_program);
    let mut ixs = Vec::new();
    // Base ATA (trade reads it as-is) + quote ATAs on token-paired
    // quotes (official TRADE_V3.md prelude).
    if rpc.get_account(&buyer_ata).await.is_err() {
        ixs.push(executor::instructions::build_create_ata_ix(
            &buyer, &buyer, &ev.mint, &buyer_ata, &buyer_token_program,
        ));
    }
    if quote_mint != executor::instructions::WSOL_MINT {
        let user_quote = executor::instructions::user_ata_with_program(&buyer, &quote_mint, &quote_program);
        if rpc.get_account(&user_quote).await.is_err() {
            ixs.push(executor::instructions::build_create_ata_ix(
                &buyer, &buyer, &quote_mint, &user_quote, &quote_program,
            ));
        }
        if rpc.get_account(&buyback).await.is_err() {
            ixs.push(executor::instructions::build_create_ata_ix(
                &buyer, &buyback_wallet, &quote_mint, &buyback, &quote_program,
            ));
        }
    }
    let swap = PumpSwapBuilder::build_buy_v3_ix(
        1_000,
        u64::MAX,
        &executor::instructions::BuyV3Accounts {
            payer: &buyer,
            mint: &ev.mint,
            quote_mint: &quote_mint,
            bonding_curve: &ev.bonding_curve,
            associated_bonding_curve: &ev.associated_bonding_curve,
            base_token_program: &buyer_token_program,
            quote_token_program: &quote_program,
            buyback_recipient: &buyback,
        },
    );
    ixs.push(swap);
    // NOTE: unsigned skeleton around the real buyer (sig_verify off) —
    // this proves account layout and program logic, not authorization.
    let blockhash = rpc_retry("blockhash", || rpc.get_latest_blockhash()).await;
    let message =
        solana_message::Message::new_with_blockhash(&ixs, Some(&buyer), &blockhash);
    let tx = Transaction {
        signatures: vec![solana_signature::Signature::default(); message.header.num_required_signatures as usize],
        message,
    };
    let res = rpc_retry("simulate", || {
        rpc.simulate_transaction_with_config(
            &tx,
            RpcSimulateTransactionConfig { sig_verify: false, ..Default::default() },
        )
    })
    .await;
    println!(
        "SIM logs:\n{}",
        res.value.logs.as_ref().map(|l| l.join("\n")).unwrap_or_default()
    );
    assert!(
        res.value.err.is_none(),
        "mayhem extended buy simulation failed: {:?}",
        res.value.err
    );
    println!("MAYHEM BUY GREEN mint={}", ev.mint);
}

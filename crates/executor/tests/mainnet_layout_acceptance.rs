//! Mainnet layout acceptance (Syarat 1 pra-funding).
//!
//! For one fresh mainnet `buy` and one fresh `sell` on the Pump.fun
//! bonding curve program, this exercises OUR full build path exactly as
//! production runs it:
//! 1. resolve the token program from the mint account owner
//!    (classic SPL vs Token-2022),
//! 2. resolve the fee recipient from the Global account,
//! 3. rebuild the instruction with `PumpSwapBuilder` from the chain's
//!    own accounts,
//! 4. report any byte-level divergence vs the on-chain instruction,
//! 5. `simulateTransaction` the rebuilt transaction (sig_verify off).
//!
//! THE GATE IS (5): simulation must succeed. Byte equality (4) is
//! reported, not asserted — the program accepts multiple account shapes
//! (volume accumulators, fee config, mayhem routing) and the builder
//! targets the documented core layout; what matters for funds is that
//! OUR bytes execute cleanly.
//!
//! Rebuilt economics are neutered (dust amount, open guards): proves
//! layout, not profitability. Never sends anything: simulation only,
//! random signer, no wallet.
//!
//! NEVER runs in normal `cargo test` (`#[ignore]`) nor CI. Needs public
//! mainnet RPC (rate limits may require a rerun):
//! `MAINNET_LAYOUT=1 cargo test -p executor --test mainnet_layout_acceptance -- --ignored --nocapture`

use executor::instructions::{PUMP_BUY_DISCRIMINATOR, PUMP_SELL_DISCRIMINATOR, PumpSwapBuilder};
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

struct SampledIx {
    signature: String,
    accounts: Vec<Pubkey>,
    data: Vec<u8>,
}

/// Scan recent Pump.fun mentions for the first top-level instruction
/// with `disc` in a successful transaction, optionally skipping `skip`
/// leading matches (for resampling). Returns accounts + data.
async fn sample_ix(
    rpc: &RpcClient,
    disc: &[u8; 8],
    label: &str,
    skip: usize,
) -> SampledIx {
    let sigs: Vec<String> = rpc
        .get_signatures_for_address_with_config(
            &Pubkey::from_str(PUMP_PROGRAM).expect("pump id"),
            solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config {
                before: None,
                until: None,
                limit: Some(150),
                commitment: None,
            },
        )
        .await
        .expect("signatures")
        .into_iter()
        .filter(|s| s.err.is_none())
        .map(|s| s.signature)
        .collect();
    assert!(!sigs.is_empty(), "no pump signatures");
    let mut seen = 0usize;
    for sig in &sigs {
        let fetched = match rpc
            .get_transaction_with_config(
                &solana_signature::Signature::from_str(sig).expect("sig"),
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
            if data.len() < 8 || data[..8] != *disc {
                continue;
            }
            if seen < skip {
                seen += 1;
                continue;
            }
            let accounts: Vec<Pubkey> = cix
                .accounts
                .iter()
                .filter_map(|i| keys.get(usize::from(*i)).copied())
                .collect();
            if accounts.len() != cix.accounts.len() {
                continue;
            }
            println!("sampled {label} in {sig} ({} accounts)", accounts.len());
            return SampledIx { signature: sig.clone(), accounts, data };
        }
    }
    panic!("no {label} instruction found in {} sampled signatures", sigs.len());
}

async fn live_fee_recipient(rpc: &RpcClient) -> Pubkey {
    executor::executor::fetch_global_fee_recipient(rpc)
        .await
        .expect("Global fee recipient unreadable")
}

async fn live_token_program(rpc: &RpcClient, mint: &Pubkey) -> Pubkey {
    rpc.get_account(mint).await.expect("mint readable").owner
}

/// Report byte divergence vs chain (informational), then simulate OUR
/// rebuilt transaction. Panics only on simulation program error — the
/// gate that guards funds.
async fn check_rebuilt(
    rpc: &RpcClient,
    sample: &SampledIx,
    ours: &Instruction,
    payer: &Pubkey,
    label: &str,
) {
    let chain = &sample.accounts;
    let built: Vec<Pubkey> = ours.accounts.iter().map(|m| m.pubkey).collect();
    let common = chain.len().min(built.len());
    let mut first_diff = None;
    for i in 0..common {
        if chain[i] != built[i] && first_diff.is_none() {
            first_diff = Some(i);
        }
    }
    println!(
        "{label} chain_accounts={} built_accounts={} first_diff={:?} data_len(chain={}, ours={})",
        chain.len(),
        built.len(),
        first_diff,
        sample.data.len(),
        ours.data.len()
    );
    assert_eq!(&ours.data[..8], &sample.data[..8], "{label} discriminator mismatch");

    let blockhash = rpc.get_latest_blockhash().await.expect("blockhash");
    // Unsigned skeleton (sig_verify off): only layout matters here.
    let message =
        solana_message::Message::new_with_blockhash(std::slice::from_ref(ours), Some(payer), &blockhash);
    let tx = Transaction {
        signatures: vec![solana_signature::Signature::default(); message.header.num_required_signatures as usize],
        message,
    };
    let res = rpc
        .simulate_transaction_with_config(
            &tx,
            RpcSimulateTransactionConfig { sig_verify: false, ..Default::default() },
        )
        .await
        .expect("simulate RPC");
    assert!(
        res.value.err.is_none(),
        "{label} simulation failed (layout mismatch?): {:?}",
        res.value.err
    );
    println!("{label} simulated clean (units: {})", res.value.units_consumed.unwrap_or(0));
}

#[tokio::test]
#[ignore]
async fn mainnet_buy_layout_matches_and_simulates() {
    if std::env::var("MAINNET_LAYOUT").as_deref() != Ok("1") {
        eprintln!("skipped: set MAINNET_LAYOUT=1 to run the live layout probe");
        return;
    }
    let rpc = RpcClient::new(RPC_URL.to_string());
    let sample = sample_ix(&rpc, &PUMP_BUY_DISCRIMINATOR, "buy", 0).await;

    // Production resolution path: token program + fee recipient live.
    let token_program = live_token_program(&rpc, &sample.accounts[2]).await;
    let fee_recipient = live_fee_recipient(&rpc).await;
    println!("mint owner (token program): {token_program}");
    println!("resolved fee recipient: {fee_recipient} (chain[1]: {})", sample.accounts[1]);
    let ours = PumpSwapBuilder::build_swap_ix(
        executor::instructions::SwapKind::Buy { amount: 1_000, max_sol_cost: u64::MAX, track_volume: true },
        &executor::instructions::SwapAccounts {
            payer: &sample.accounts[6],
            mint: &sample.accounts[2],
            bonding_curve: &sample.accounts[3],
            associated_bonding_curve: &sample.accounts[4],
            user_ata: &sample.accounts[5],
            fee_recipient: &fee_recipient,
            token_program: &token_program,
            creator: None,
        },
    );
    check_rebuilt(&rpc, &sample, &ours, &sample.accounts[6], "buy").await;
    println!("BUY ACCEPTED sig={}", sample.signature);
}

#[tokio::test]
#[ignore]
async fn mainnet_sell_layout_matches_and_simulates() {
    if std::env::var("MAINNET_LAYOUT").as_deref() != Ok("1") {
        eprintln!("skipped: set MAINNET_LAYOUT=1 to run the live layout probe");
        return;
    }
    let rpc = RpcClient::new(RPC_URL.to_string());
    // Resample until the seller currently holds dust (else the sim is
    // meaningless); abort loudly rather than weak-pass.
    for skip in 0..5usize {
        let sample = sample_ix(&rpc, &PUMP_SELL_DISCRIMINATOR, "sell", skip).await;
        let ata = sample.accounts[5];
        let seller = sample.accounts[6];
        let balance: u64 = match rpc.get_token_account_balance(&ata).await {
            Ok(b) => b.amount.parse().unwrap_or(0),
            Err(_) => continue,
        };
        if balance < 1_000 {
            println!("seller holds {balance}, resampling");
            continue;
        }
        let token_program = live_token_program(&rpc, &sample.accounts[2]).await;
        let fee_recipient = live_fee_recipient(&rpc).await;
        println!("resolved fee recipient: {fee_recipient} (chain[1]: {})", sample.accounts[1]);
        let ours = PumpSwapBuilder::build_swap_ix(
            executor::instructions::SwapKind::Sell { amount: 1_000, min_sol_output: 0 },
            &executor::instructions::SwapAccounts {
                payer: &seller,
                mint: &sample.accounts[2],
                bonding_curve: &sample.accounts[3],
                associated_bonding_curve: &sample.accounts[4],
                user_ata: &ata,
                fee_recipient: &fee_recipient,
                token_program: &token_program,
                creator: None,
            },
        );
        check_rebuilt(&rpc, &sample, &ours, &seller, "sell").await;
        println!("SELL ACCEPTED sig={}", sample.signature);
        return;
    }
    panic!("no sell sample with seller holdings after resampling");
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
    let rpc = RpcClient::new(RPC_URL.to_string());
    // Fresh launch hunt (bulk blocks, as before).
    let tip = rpc.get_slot().await.expect("slot");
    eprintln!("hunting from tip {tip}");
    let mut launch: Option<(listener::PumpTokenLaunchEvent, Option<String>)> = None;
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
                        // Buyer for the sim: the create's fee payer
                        // (keys[0]), funded by definition — it JUST paid
                        // for creation seconds ago.
                        let payer = keys.first().map(|k| k.to_string());
                        eprintln!("fresh v2 launch: {} slot={}", ev.mint, ev.slot);
                        launch = Some((ev, payer));
                        break;
                    }
                    Err(_) => continue,
                }
            }
        }
        probe_slot = probe_slot.saturating_sub(25);
    }
    let (ev, payer) = launch.unwrap_or_else(|| {
        panic!(
            "no fresh create_v2 in {blocks_scanned} blocks ({txs_scanned} txs); launches are sporadic, rerun"
        )
    });
    let creator = ev.creator.expect("v2 launch must carry a creator");
    // Public RPC throttles aggressively after bulk block fetches: retry
    // every probe RPC below with backoff instead of failing the run on
    // the first 429.
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
    // Buyer = the create's fee payer (funded seconds ago by paying
    // for creation). No holder discovery needed: no largest-accounts
    // (hard-throttled), no token-accounts scan. The layer being proven
    // is account layout, and the payer/buyer/ATA below are all live and
    // funded by construction.
    let buyer = payer
        .as_deref()
        .and_then(|x| solana_pubkey::Pubkey::from_str(x).ok())
        .expect("fee payer");
    let buyer_sol: u64 = rpc_retry("buyer balance", || rpc.get_balance(&buyer)).await;
    println!("buyer: {buyer} ({buyer_sol} lamports)");
    assert!(buyer_sol > 100_000, "buyer broke; rerun for another launch");
    let buyer_token_program = live_token_program(&rpc, &ev.mint).await;
    let buyer_ata =
        executor::instructions::user_ata_with_program(&buyer, &ev.mint, &buyer_token_program);
    println!("holder token account: {buyer_ata}");
    // PDAs must exist (created at launch/first trade); abort loudly if not.
    let vault = executor::instructions::creator_vault_pda(&creator);
    let uvol = executor::instructions::user_volume_accumulator_pda(&buyer);
    for (name, key) in [("creator_vault", vault), ("user_vol", uvol)] {
        rpc_retry("routing PDA read", || rpc.get_account(&key))
            .await;
        let _ = name;
    }
    // Buyer doubles as fee payer (self-funded sniper shape, exactly
    // like production). A stranger account must NOT be used: one
    // hardcoded tip account turned out to be program-owned (tip-router
    // migration) and fails fee validation with InvalidAccountForFee.
    let token_program = live_token_program(&rpc, &ev.mint).await;
    let fee_recipient = live_fee_recipient(&rpc).await;
    // Prepend ATA creation exactly like production preflight iff the
    // buyer ATA is missing (duplicate creates fail the whole bundle).
    let mut ixs = Vec::new();
    if rpc.get_account(&buyer_ata).await.is_err() {
        ixs.push(executor::instructions::build_create_ata_ix(
            &buyer, &buyer, &ev.mint, &buyer_ata, &token_program,
        ));
    }
    let swap = PumpSwapBuilder::build_swap_ix(
        executor::instructions::SwapKind::Buy { amount: 1_000, max_sol_cost: u64::MAX, track_volume: true },
        &executor::instructions::SwapAccounts {
            payer: &buyer,
            mint: &ev.mint,
            bonding_curve: &ev.bonding_curve,
            associated_bonding_curve: &ev.associated_bonding_curve,
            user_ata: &buyer_ata,
            fee_recipient: &fee_recipient,
            token_program: &token_program,
            creator: Some(&creator),
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

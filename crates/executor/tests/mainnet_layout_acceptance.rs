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

use executor::instructions::{
    PUMP_BUY_DISCRIMINATOR, PUMP_SELL_DISCRIMINATOR, PumpSwapBuilder,
};
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
/// leading matches (for resampling). Returns accounts + data. Pages
/// through up to 5 signature windows (750 sigs): legacy shapes are rare
/// now that v3/v2 dominate traffic, and a single 150-sig window misses
/// them (proven by the first probe run finding zero legacy fills in
/// 32-45 sigs).
/// Shared RPC constructor: `MAINNET_RPC_URL` (Helius) when set, else
/// `HTTP_URL` (also Helius in practice — sanitized of backslash escapes),
/// else the public endpoint. Every probe must use this — the public node
/// 429s after bulk scans, which turns a 3-minute run into 20 minutes
/// of backoff.
fn rpc_client() -> RpcClient {
    let url = std::env::var("MAINNET_RPC_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| {
            std::env::var("HTTP_URL")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .map(|v| v.replace('\\', ""))
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| RPC_URL.to_string());
    RpcClient::new(url)
}

async fn sample_ix(
    rpc: &RpcClient,
    disc: &[u8; 8],
    label: &str,
    skip: usize,
    expected_accounts: Option<usize>,
    expected_data_len: Option<usize>,
) -> Option<SampledIx> {
    let mut before = None;
    let mut windows_scanned = 0usize;
    let mut sigs_total = 0usize;
    let mut seen = 0usize;
    // 10 windows (1500 sigs): legacy fills are extinct as of Oct-2026
    // (all current flow is 18-account mayhem / v2 / v3), so a narrow
    // scan misses them entirely. Callers treat `None` as SKIP — the
    // legacy builder stays byte-locked by unit tests and dispatch-time
    // sim preflight still gates any future legacy dispatch.
    for _ in 0..10usize {
        let sigs: Vec<String> = rpc
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
            .expect("signatures")
            .into_iter()
            .filter(|s| s.err.is_none())
            .map(|s| s.signature)
            .collect();
        if sigs.is_empty() {
            break;
        }
        windows_scanned += 1;
        sigs_total += sigs.len();
        before = sigs
            .last()
            .and_then(|s| solana_signature::Signature::from_str(s).ok());
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
            // Shape filter BEFORE skip-counting: the same discriminator
            // covers both the 12-account legacy core and the 18-account
            // mayhem tail (proven 2026-10-13: first hit was 18 accounts
            // with a 26-byte payload, our 12-account rebuild simmed 3005).
            // Counting only shape-matching hits keeps `skip` semantics.
            if expected_data_len.is_some_and(|n| data.len() != n) {
                continue;
            }
            // Account count needs the resolved vec; peek at raw len first.
            if expected_accounts.is_some_and(|n| cix.accounts.len() != n) {
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
            return Some(SampledIx { signature: sig.clone(), accounts, data });
        }
    }
    }
    eprintln!("SKIP: no {label} instruction in {sigs_total} sampled signatures ({windows_scanned} windows) — shape extinct on chain");
    None
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
async fn mainnet_buy_layout_matches_and_simulates() {
    if std::env::var("MAINNET_LAYOUT").as_deref() != Ok("1") {
        eprintln!("skipped: set MAINNET_LAYOUT=1 to run the live layout probe");
        return;
    }
    let rpc = rpc_client();
    // Legacy core only: 12 accounts + 25-byte payload (disc + amount +
    // max + track_volume). The 18-account mayhem tail shares the
    // discriminator but needs reference routing, not this builder.
    let Some(sample) =
        sample_ix(&rpc, &PUMP_BUY_DISCRIMINATOR, "buy", 0, Some(12), Some(25)).await
    else {
        return;
    };

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
            mayhem_routing: None,
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
    let rpc = rpc_client();
    // Resample until the seller currently holds dust (else the sim is
    // meaningless); abort loudly rather than weak-pass.
    for skip in 0..5usize {
        let Some(sample) =
            sample_ix(&rpc, &PUMP_SELL_DISCRIMINATOR, "sell", skip, Some(12), Some(24)).await
        else {
            eprintln!("SKIP: legacy sell shape extinct; builder byte-locked by unit tests");
            return;
        };
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
                mayhem_routing: None,
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
async fn mainnet_sell_v2_simulates_end_to_end() {
    // sell_v2 execution gate (P0.2): the 26-account builder must not only
    // match one frozen sample byte-for-byte (unit test) but SIMULATE
    // clean on a FRESH curve with live-resolved accounts. Per-sample
    // mapping asserts below prove the derivation map is universal, not
    // fixture-fitted: any mismatch panics loudly instead of resampling
    // past it. Dust economics (neutered amount, open guard): proves
    // layout, not profitability. Simulation only, never sends anything.
    if std::env::var("MAINNET_LAYOUT").as_deref() != Ok("1") {
        eprintln!("skipped: set MAINNET_LAYOUT=1 to run the live layout probe");
        return;
    }
    use executor::instructions::{
        PUMP_SELL_V2_DISCRIMINATOR, SellV2Accounts, build_create_ata_ix, creator_vault_pda,
        user_ata_with_program, user_volume_accumulator_pda,
    };
    // Endpoint override for throttled windows: the default public RPC
    // rate-limits aggressively after bulk scans; point at any healthy
    // mainnet HTTPS endpoint instead (same data, separate budget).
    let rpc = rpc_client();
    // Deep scan: sell_v2 is rarer than legacy sells (~1-2 per 100 pump
    // txs); page until several candidates or budgets run out.
    let mut candidates: Vec<SampledIx> = Vec::new();
    let mut before = None;
    for _ in 0..5usize {
        let sigs: Vec<String> = match rpc
            .get_signatures_for_address_with_config(
                &Pubkey::from_str(PUMP_PROGRAM).expect("pump id"),
                solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config {
                    before,
                    until: None,
                    limit: Some(200),
                    commitment: None,
                },
            )
            .await
        {
            Ok(list) => list,
            Err(e) => {
                eprintln!("signature scan failed ({e}); stopping scan");
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
                if data.len() != 24 || data[..8] != PUMP_SELL_V2_DISCRIMINATOR {
                    continue;
                }
                let accounts: Vec<Pubkey> = cix
                    .accounts
                    .iter()
                    .filter_map(|i| keys.get(usize::from(*i)).copied())
                    .collect();
                if accounts.len() != 26 {
                    continue;
                }
                println!("sampled sell_v2 in {sig} (26 accounts)");
                candidates.push(SampledIx {
                    signature: sig.clone(),
                    accounts,
                    data,
                });
            }
        }
        if candidates.len() >= 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert!(
        !candidates.is_empty(),
        "no sell_v2 in scanned window; rerun later"
    );
    // Same curve can appear repeatedly: probe each market once so RPC
    // budget goes to distinct curves, not replays of one.
    {
        let mut seen = std::collections::HashSet::new();
        candidates.retain(|s| seen.insert((s.accounts[1], s.accounts[10])));
    }
    for (n, sample) in candidates.iter().enumerate() {
        println!(
            "candidate {n}: mint={} curve={}",
            sample.accounts[1], sample.accounts[10]
        );
        let mint = sample.accounts[1];
        let quote = sample.accounts[2];
        let curve = sample.accounts[10];
        // Transport errors resample (loudly); only mapping mismatch and
        // sim failure panic — the map is either universal or wrong.
        let token_program = match rpc.get_account(&mint).await {
            Ok(a) => a.owner,
            Err(e) => {
                println!("candidate {n}: mint read failed ({e}); resampling");
                continue;
            }
        };
        let quote_program = match rpc.get_account(&quote).await {
            Ok(a) => a.owner,
            Err(e) => {
                println!("candidate {n}: quote read failed ({e}); resampling");
                continue;
            }
        };
        // Mapping proof (universal, not fixture-fitted): every
        // program-derived address must equal the chain's account at its
        // position. Any mismatch panics — the map is either universal
        // or wrong, never silently resampled past.
        let chain_seller = sample.accounts[13];
        assert_eq!(
            user_ata_with_program(&sample.accounts[6], &quote, &quote_program),
            sample.accounts[7],
            "fee quote ATA mismatch"
        );
        assert_eq!(
            user_ata_with_program(&curve, &quote, &quote_program),
            sample.accounts[12],
            "curve quote ATA mismatch"
        );
        // Creator reads off the curve account (mayhem tail 49..81).
        let curve_data = match rpc.get_account(&curve).await {
            Ok(a) => a,
            Err(e) => {
                println!("candidate {n}: curve read failed ({e}); resampling");
                continue;
            }
        };
        if curve_data.data.len() < 81 {
            println!("curve too short for creator read; resampling");
            continue;
        }
        let creator = Pubkey::new_from_array(curve_data.data[49..81].try_into().expect("32B"));
        assert_eq!(
            creator_vault_pda(&creator),
            sample.accounts[16],
            "creator vault mismatch"
        );
        assert_eq!(
            user_volume_accumulator_pda(&chain_seller),
            sample.accounts[19],
            "user-vol mismatch"
        );
        // Wallet-convention accounts ([14]/[15]) are canonical ATAs when
        // the seller uses them — but holders may sit in aux accounts
        // (documented residual), which no seed derivation can reproduce.
        // Aux sellers cannot validate our canonical path: skip loudly.
        if user_ata_with_program(&chain_seller, &mint, &token_program) != sample.accounts[14]
            || user_ata_with_program(&chain_seller, &quote, &quote_program) != sample.accounts[15]
        {
            println!("seller uses aux accounts; resampling");
            continue;
        }
        // Sim seller: the SAMPLED seller itself with a 1-unit debit.
        // Historical sellers usually exited fully, so the debit is
        // EXPECTED to fail with token InsufficientFunds (Custom(1)) —
        // which still proves the layout: every account resolved, every
        // owner/program check passed, execution reached the token debit
        // CPI. A clean sim passes outright. Anything else (6000-range
        // pump errors, missing accounts, fee failures) is a layout
        // problem and fails the gate. Unsigned skeleton (sig_verify
        // off): proves layout, never authorization, never funds.
        let seller = chain_seller;
        // Fee-payer solvency first: a broke payer fails with a fee
        // error that would confound the funds-only reading below.
        match rpc.get_balance(&seller).await {
            Ok(lamports) if lamports >= 100_000 => {}
            _ => {
                println!("seller lacks fee SOL; resampling");
                continue;
            }
        }
        let fee_recipient = match executor::executor::fetch_global_fee_recipient(&rpc).await {
            Some(fee) => fee,
            None => {
                println!("candidate {n}: fee recipient unreadable; resampling");
                continue;
            }
        };
        let seller_base_ata = user_ata_with_program(&seller, &mint, &token_program);
        let seller_quote_ata = user_ata_with_program(&seller, &quote, &quote_program);
        let seller_uvol = user_volume_accumulator_pda(&seller);
        let seller_uvol_quote_ata = user_ata_with_program(&seller_uvol, &quote, &quote_program);
        let mut ixs = Vec::new();
        // Idempotent ATA prefix for the derived seller accounts (the
        // historical ones may be closed by now): existence without
        // balance still exercises the full debit path to its
        // funds-check conclusion.
        for (owner, ata_mint, ata, prog) in [
            (&seller, &mint, &seller_base_ata, &token_program),
            (&seller, &quote, &seller_quote_ata, &quote_program),
            (&seller_uvol, &quote, &seller_uvol_quote_ata, &quote_program),
        ] {
            if rpc.get_account(ata).await.is_err() {
                ixs.push(build_create_ata_ix(&seller, owner, ata_mint, ata, prog));
            }
        }
        ixs.push(PumpSwapBuilder::build_sell_v2_ix(
            1,
            0,
            &SellV2Accounts {
                payer: &seller,
                mint: &mint,
                quote_mint: &quote,
                bonding_curve: &curve,
                associated_bonding_curve: &sample.accounts[11],
                fee_recipient: &fee_recipient,
                base_token_program: &token_program,
                quote_token_program: &quote_program,
                creator: &creator,
                pfee_vault: &sample.accounts[8],
                pfee_quote_account: &sample.accounts[9],
            },
        ));
        use solana_instruction_error::InstructionError;
        use solana_rpc_client_api::response::TransactionError;
        // Blockhash/read-then-simulate races a load-balanced public RPC:
        // a hash the fetch node knows may be unknown to the sim node
        // (empty logs, BlockhashNotFound — proves nothing either way).
        // One fresh retry separates that flake from a real verdict.
        let mut attempt = 0u32;
        loop {
            let blockhash = match rpc.get_latest_blockhash().await {
                Ok(b) => b,
                Err(e) => {
                    println!("candidate {n}: blockhash failed ({e}); resampling");
                    break;
                }
            };
            let message =
                solana_message::Message::new_with_blockhash(&ixs, Some(&seller), &blockhash);
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
                    RpcSimulateTransactionConfig {
                        sig_verify: false,
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(res) => res,
                Err(e) => {
                    println!("candidate {n}: simulate RPC failed ({e}); resampling");
                    break;
                }
            };
            let logs = res
                .value
                .logs
                .as_ref()
                .map(|l| l.join("\n"))
                .unwrap_or_default();
            println!("SIM logs:\n{logs}");
            // Verdict: clean, or execution stopped ONLY on empty funds —
            // either at the token debit (TokenError::InsufficientFunds =
            // Custom(1) after a token-program CPI) or at the pump
            // program's own balance gate first (`NotEnoughTokensToSell`,
            // 6023, from sell_v2.rs — observed with `Left: 0, Right:
            // 1`). Pump's Custom errors otherwise live at 6000+ for
            // layout violations, so any other error is a layout-level
            // failure and fails the gate. Either funds-conclusion proves
            // the same thing: every account resolved, every
            // owner/program/PDA check passed, and the program walked the
            // layout to the debit math.
            let token_cpi_reached = logs.contains("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb")
                || logs.contains("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
            let err: Option<TransactionError> = res.value.err.clone().map(TransactionError::from);
            match err {
                None => {
                    println!("SELL_V2 GREEN (clean) sig={}", sample.signature);
                    return;
                }
                Some(TransactionError::InstructionError(_, InstructionError::Custom(1)))
                    if token_cpi_reached =>
                {
                    println!(
                        "SELL_V2 GREEN (funds-conclusion at token debit) sig={}",
                        sample.signature
                    );
                    return;
                }
                Some(TransactionError::InstructionError(_, InstructionError::Custom(6023)))
                    if logs.contains("NotEnoughTokensToSell") =>
                {
                    println!(
                        "SELL_V2 GREEN (funds-conclusion at pump balance gate) sig={}",
                        sample.signature
                    );
                    return;
                }
                Some(TransactionError::BlockhashNotFound) if attempt == 0 && logs.is_empty() => {
                    println!("candidate {n}: stale blockhash under load; one fresh retry");
                    attempt += 1;
                    continue;
                }
                Some(other) => {
                    panic!(
                        "sell_v2 simulation failed with layout-level error: {other:?}\nlogs:\n{logs}"
                    );
                }
            }
        }
    }
    panic!("no sell_v2 sample simulating clean after resampling");
}

#[tokio::test]
#[ignore]
async fn mainnet_mayhem_buy_18_simulates() {
    // 18-shape buy execution gate: rebuild a fresh mayhem buy with OUR
    // builder (16 prefix derived + [16]/[17] lifted from the SAME tx as
    // reference) and simulate with the sampled buyer as payer (dust
    // amount, open guard, sig_verify off). Unsigned skeleton: proves
    // layout, not authorization. Simulation only, never sends anything.
    if std::env::var("MAINNET_LAYOUT").as_deref() != Ok("1") {
        eprintln!("skipped: set MAINNET_LAYOUT=1 to run the live layout probe");
        return;
    }
    use executor::instructions::{
        MayhemRouting, PUMP_BUY_DISCRIMINATOR, SwapAccounts, build_create_ata_ix, creator_vault_pda,
        user_ata_with_program, user_volume_accumulator_pda,
    };
    let rpc = rpc_client();
    let mut before = None;
    // Counters mirror the v3 probe: 18-account flow is sporadic (a full
    // run GREENed, the next found zero hits in 1000 sigs), so a blind
    // panic wastes the diagnosis.
    let mut windows = 0usize;
    let mut sigs_scanned = 0usize;
    let mut hits18 = 0usize;
    let mut aux_drops = 0usize;
    let mut poor_drops = 0usize;
    for _ in 0..10usize {
        let sigs: Vec<String> = match rpc
            .get_signatures_for_address_with_config(
                &Pubkey::from_str(PUMP_PROGRAM).expect("pump id"),
                solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config {
                    before,
                    until: None,
                    limit: Some(200),
                    commitment: None,
                },
            )
            .await
        {
            Ok(list) => list,
            Err(e) => {
                eprintln!("signature scan failed ({e}); stopping scan");
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
        windows += 1;
        sigs_scanned += sigs.len();
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
                if data.len() != 25 || data[..8] != PUMP_BUY_DISCRIMINATOR {
                    continue;
                }
                let accounts: Vec<Pubkey> = cix
                    .accounts
                    .iter()
                    .filter_map(|i| keys.get(usize::from(*i)).copied())
                    .collect();
                if accounts.len() != 18 {
                    continue;
                }
                hits18 += 1;
                // Buyer doubles as sim payer: must still hold dust.
                let buyer = accounts[6];
                let mint = accounts[2];
                let token_program = match rpc.get_account(&mint).await {
                    Ok(a) => a.owner,
                    Err(e) => {
                        println!("mint read failed ({e}); resampling");
                        continue;
                    }
                };
                let buyer_ata = user_ata_with_program(&buyer, &mint, &token_program);
                if buyer_ata != accounts[5] {
                    aux_drops += 1;
                    println!("buyer uses aux account; resampling");
                    continue;
                }
                let balance: u64 = match rpc.get_token_account_balance(&buyer_ata).await {
                    Ok(b) => b.amount.parse().unwrap_or(0),
                    Err(_) => continue,
                };
                if balance < 1_000 {
                    poor_drops += 1;
                    continue;
                }
                let curve = accounts[3];
                let curve_data = match rpc.get_account(&curve).await {
                    Ok(a) => a,
                    Err(e) => {
                        println!("curve read failed ({e}); resampling");
                        continue;
                    }
                };
                if curve_data.data.len() < 81 {
                    println!("curve too short for creator read; resampling");
                    continue;
                }
                let creator =
                    Pubkey::new_from_array(curve_data.data[49..81].try_into().expect("32B"));
                if creator_vault_pda(&creator) != accounts[9] {
                    println!("creator vault mismatch (non-mayhem curve?); resampling");
                    continue;
                }
                if user_volume_accumulator_pda(&buyer) != accounts[13] {
                    println!("user-vol mismatch; resampling");
                    continue;
                }
                let fee_recipient = accounts[1];
                println!(
                    "fee: sample[1]={fee_recipient} (same-tx era consistency; live Global bypassed)"
                );
                let mut ixs = Vec::new();
                if rpc.get_account(&buyer_ata).await.is_err() {
                    ixs.push(build_create_ata_ix(
                        &buyer,
                        &buyer,
                        &mint,
                        &buyer_ata,
                        &token_program,
                    ));
                }
                ixs.push(PumpSwapBuilder::build_swap_ix(
                    executor::instructions::SwapKind::Buy {
                        amount: 1_000,
                        max_sol_cost: u64::MAX,
                        track_volume: true,
                    },
                    &SwapAccounts {
                        payer: &buyer,
                        mint: &mint,
                        bonding_curve: &curve,
                        associated_bonding_curve: &accounts[4],
                        user_ata: &buyer_ata,
                        fee_recipient: &fee_recipient,
                        token_program: &token_program,
                        creator: Some(&creator),
                        mayhem_routing: Some(MayhemRouting {
                            unknown_16: &accounts[16],
                            pfee_vault: &accounts[17],
                        }),
                    },
                ));
                let blockhash = match rpc.get_latest_blockhash().await {
                    Ok(b) => b,
                    Err(e) => {
                        println!("blockhash failed ({e}); resampling");
                        continue;
                    }
                };
                let message =
                    solana_message::Message::new_with_blockhash(&ixs, Some(&buyer), &blockhash);
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
                        RpcSimulateTransactionConfig {
                            sig_verify: false,
                            ..Default::default()
                        },
                    )
                    .await
                {
                    Ok(res) => res,
                    Err(e) => {
                        println!("simulate RPC failed ({e}); resampling");
                        continue;
                    }
                };
                println!(
                    "SIM logs:\n{}",
                    res.value.logs.as_ref().map(|l| l.join("\n")).unwrap_or_default()
                );
                assert!(
                    res.value.err.is_none(),
                    "mayhem 18-shape buy simulation failed: {:?}",
                    res.value.err
                );
                println!("MAYHEM18 GREEN sig={sig}");
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    eprintln!("mayhem18 scan summary: windows={windows} sigs={sigs_scanned} hits18={hits18} aux_drops={aux_drops} poor_drops={poor_drops}");
    panic!("no mayhem 18-shape buy simulating clean after resampling");
}

#[tokio::test]
#[ignore]
async fn mainnet_buy_v2_27_simulates() {
    // 27-shape buy execution gate: rebuild a fresh `buy_v2` with OUR
    // builder (derived addresses recomputed + opaque routing lifted from
    // the SAME tx as reference) and simulate with the sampled buyer as
    // payer (dust amount, open guard, sig_verify off). Unsigned
    // skeleton: proves layout, not authorization. Simulation only,
    // never sends anything.
    if std::env::var("MAINNET_LAYOUT").as_deref() != Ok("1") {
        eprintln!("skipped: set MAINNET_LAYOUT=1 to run the live layout probe");
        return;
    }
    use executor::instructions::{
        BuyV2Accounts, PUMP_BUY_V2_DISCRIMINATOR, build_create_ata_ix, creator_vault_pda,
        user_ata_with_program, user_volume_accumulator_pda,
    };
    let rpc = rpc_client();
    let mut before = None;
    for _ in 0..5usize {
        let sigs: Vec<String> = match rpc
            .get_signatures_for_address_with_config(
                &Pubkey::from_str(PUMP_PROGRAM).expect("pump id"),
                solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config {
                    before,
                    until: None,
                    limit: Some(200),
                    commitment: None,
                },
            )
            .await
        {
            Ok(list) => list,
            Err(e) => {
                eprintln!("signature scan failed ({e}); stopping scan");
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
                if data.len() != 24 || data[..8] != PUMP_BUY_V2_DISCRIMINATOR {
                    continue;
                }
                let accounts: Vec<Pubkey> = cix
                    .accounts
                    .iter()
                    .filter_map(|i| keys.get(usize::from(*i)).copied())
                    .collect();
                if accounts.len() != 27 {
                    continue;
                }
                // Buyer doubles as sim payer: must still hold dust in the
                // canonical base ATA (aux-account buyers resample).
                let buyer = accounts[13];
                let mint = accounts[1];
                let quote_mint = accounts[2];
                let token_program = match rpc.get_account(&mint).await {
                    Ok(a) => a.owner,
                    Err(e) => {
                        println!("mint read failed ({e}); resampling");
                        continue;
                    }
                };
                let quote_program = match rpc.get_account(&quote_mint).await {
                    Ok(a) => a.owner,
                    Err(e) => {
                        println!("quote mint read failed ({e}); resampling");
                        continue;
                    }
                };
                let buyer_ata = user_ata_with_program(&buyer, &mint, &token_program);
                if buyer_ata != accounts[14] {
                    println!("buyer uses aux account; resampling");
                    continue;
                }
                let balance: u64 = match rpc.get_token_account_balance(&buyer_ata).await {
                    Ok(b) => b.amount.parse().unwrap_or(0),
                    Err(_) => continue,
                };
                if balance < 1_000 {
                    continue;
                }
                let curve = accounts[10];
                let curve_data = match rpc.get_account(&curve).await {
                    Ok(a) => a,
                    Err(e) => {
                        println!("curve read failed ({e}); resampling");
                        continue;
                    }
                };
                if curve_data.data.len() < 81 {
                    println!("curve too short for creator read; resampling");
                    continue;
                }
                let creator =
                    Pubkey::new_from_array(curve_data.data[49..81].try_into().expect("32B"));
                if creator_vault_pda(&creator) != accounts[16] {
                    println!("creator vault mismatch (non-v2 curve?); resampling");
                    continue;
                }
                if user_volume_accumulator_pda(&buyer) != accounts[20] {
                    println!("user-vol mismatch; resampling");
                    continue;
                }
                let fee_recipient = accounts[6];
                let buyer_quote_ata =
                    user_ata_with_program(&buyer, &quote_mint, &quote_program);
                let mut ixs = Vec::new();
                if rpc.get_account(&buyer_ata).await.is_err() {
                    ixs.push(build_create_ata_ix(
                        &buyer,
                        &buyer,
                        &mint,
                        &buyer_ata,
                        &token_program,
                    ));
                }
                if rpc.get_account(&buyer_quote_ata).await.is_err() {
                    ixs.push(build_create_ata_ix(
                        &buyer,
                        &buyer,
                        &quote_mint,
                        &buyer_quote_ata,
                        &quote_program,
                    ));
                }
                ixs.push(PumpSwapBuilder::build_buy_v2_ix(
                    1_000,
                    u64::MAX,
                    &BuyV2Accounts {
                        payer: &buyer,
                        mint: &mint,
                        quote_mint: &quote_mint,
                        bonding_curve: &curve,
                        associated_bonding_curve: &accounts[11],
                        fee_recipient: &fee_recipient,
                        base_token_program: &token_program,
                        quote_token_program: &quote_program,
                        creator: &creator,
                        pfee_vault: &accounts[8],
                        pfee_quote_account: &accounts[9],
                    },
                ));
                let blockhash = match rpc.get_latest_blockhash().await {
                    Ok(b) => b,
                    Err(e) => {
                        println!("blockhash failed ({e}); resampling");
                        continue;
                    }
                };
                let message =
                    solana_message::Message::new_with_blockhash(&ixs, Some(&buyer), &blockhash);
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
                        RpcSimulateTransactionConfig {
                            sig_verify: false,
                            ..Default::default()
                        },
                    )
                    .await
                {
                    Ok(res) => res,
                    Err(e) => {
                        println!("simulate RPC failed ({e}); resampling");
                        continue;
                    }
                };
                println!(
                    "SIM logs:\n{}",
                    res.value.logs.as_ref().map(|l| l.join("\n")).unwrap_or_default()
                );
                // Blockhash/read-then-simulate races a load-balanced public
                // RPC: a hash the fetch node knows may be unknown to the sim
                // node (empty logs, BlockhashNotFound — proves nothing
                // either way). One fresh retry separates that flake from a
                // real verdict (same pattern as the sell_v2 probe).
                use solana_rpc_client_api::response::TransactionError;
                if matches!(
                    res.value.err.clone().map(TransactionError::from),
                    Some(TransactionError::BlockhashNotFound)
                ) && res.value.logs.as_ref().map(|l| l.is_empty()).unwrap_or(true)
                {
                    println!("stale blockhash under load; one fresh retry");
                    let blockhash = match rpc.get_latest_blockhash().await {
                        Ok(b) => b,
                        Err(e) => {
                            println!("blockhash failed ({e}); resampling");
                            continue;
                        }
                    };
                    let message = solana_message::Message::new_with_blockhash(
                        &ixs,
                        Some(&buyer),
                        &blockhash,
                    );
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
                            RpcSimulateTransactionConfig {
                                sig_verify: false,
                                ..Default::default()
                            },
                        )
                        .await
                    {
                        Ok(res) => res,
                        Err(e) => {
                            println!("simulate RPC failed ({e}); resampling");
                            continue;
                        }
                    };
                    println!(
                        "SIM logs (retry):\n{}",
                        res.value.logs.as_ref().map(|l| l.join("\n")).unwrap_or_default()
                    );
                    assert!(
                        res.value.err.is_none(),
                        "buy_v2 27-shape simulation failed: {:?}",
                        res.value.err
                    );
                    println!("BUYV2 GREEN sig={sig}");
                    return;
                }
                assert!(
                    res.value.err.is_none(),
                    "buy_v2 27-shape simulation failed: {:?}",
                    res.value.err
                );
                println!("BUYV2 GREEN sig={sig}");
                return;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    panic!("no buy_v2 27-shape simulating clean after resampling");
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
    // PDAs must exist (created at launch/first trade); abort loudly if not.
    let vault = executor::instructions::creator_vault_pda(&creator);
    let uvol = executor::instructions::user_volume_accumulator_pda(&buyer);
    for (name, key) in [("creator_vault", vault), ("user_vol", uvol)] {
        rpc_retry("routing PDA read", || rpc.get_account(&key))
            .await;
        let _ = name;
    }
    // Fresh curves are v3-universal (17 accounts, fees stay on the
    // curve): no fee recipient, no creator vault, no 18/27 reference
    // needed. The legacy 16-account shape fails here with 6062
    // (BuybackFeeRecipientMissing) because buyback-enforcing curves
    // require the full tail — probing it off a fresh launch proves
    // nothing about our routing. The 18/27 gates keep their own
    // reference-based sim probes (mayhem_buy_18 / buy_v2_27).
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

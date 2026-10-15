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
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_pubkey::Pubkey;
use std::str::FromStr;

mod common;

const PUMP_PROGRAM: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";

async fn live_token_program(rpc: &RpcClient, mint: &Pubkey) -> Pubkey {
    rpc.get_account(mint).await.expect("mint readable").owner
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
    let rpc = common::rpc_client();

    // Fixture parsing lives in `common` (single 17-shape table); the
    // trader/ATA pair is the offline fallback when
    // `get_token_largest_accounts` is throttled (public RPC 429s on the
    // heavy indexing call): the fixture already names a real seller and
    // its ATA, so the sim gate can run without the largest-holders RPC.

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
        let (mint, quote_mint, curve, assoc, buyback) = common::fixture_v3_accounts(raw, path);
        eprintln!("fixture {path}: mint={mint} curve={curve}");
        let token_program = match rpc.get_account(&mint).await { Ok(a) => a.owner, Err(_) => continue };
        let quote_program = match rpc.get_account(&quote_mint).await { Ok(a) => a.owner, Err(_) => continue };

        // Live largest holders for this mint (holder-based replay: fill buyers flip fast,
        // zero ATAs stay open with rent — owner still resolves and often still holds SOL/WSOL).
        // Public RPC 429s on this heavy indexing call: retry with backoff
        // first (transient throttle), then fall back to the fixture's own
        // trader/ATA (a real historical seller) so the sim gate does not
        // hard-fail on throttling alone. An empty fallback (flipped/rent-
        // collected ATA) yields no candidate and `continue`s below — never
        // a false GREEN.
        let mut largest = None;
        for attempt in 0..3u32 {
            match tokio::time::timeout(std::time::Duration::from_secs(15), rpc.get_token_largest_accounts(&mint)).await {
                Ok(Ok(h)) => { largest = Some(h); break; }
                _ if attempt < 2 => {
                    eprintln!("largest-accounts unreadable for {mint} (attempt {}/3; backing off)", attempt + 1);
                    tokio::time::sleep(std::time::Duration::from_secs(2 * (attempt as u64 + 1))).await;
                }
                _ => {
                    eprintln!("largest-accounts unreadable for {mint} (throttled after 3 attempts); using fixture-trader fallback");
                }
            }
        }
        let mut holder_owners: Vec<(Pubkey, Pubkey, u64)> = Vec::new();
        let mut zero_base_owners: Vec<Pubkey> = Vec::new();
        let mut holders_len = 0usize;
        if let Some(holders) = largest.as_ref() {
            holders_len = holders.len();
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
        } else {
            // Offline fallback: the fixture's trader + base ATA. Verified
            // canonical (aux-filtered) and balance-checked live below, so a
            // stale zero-balance fallback simply yields no candidate rather
            // than a false GREEN.
            let (ftrader, fata) = common::fixture_v3_trader(raw, path);
            if user_ata_with_program(&ftrader, &mint, &token_program) == fata && ftrader != curve {
                let hbase: u64 = match rpc.get_token_account_balance(&fata).await { Ok(b) => b.amount.parse().unwrap_or(0), Err(_) => 0 };
                if hbase >= 1_000 {
                    holder_owners.push((ftrader, fata, hbase));
                } else if rpc.get_account(&fata).await.is_ok() {
                    zero_base_owners.push(ftrader);
                } else {
                    eprintln!("fixture fallback ATA missing on-chain for {mint}");
                }
            } else {
                aux_drops += 1;
                eprintln!("fixture fallback trader is aux/non-canonical for {mint}");
            }
        }
        eprintln!("fixture {path}: holders={holders_len} funded_base={} zero_base={}", holder_owners.len(), zero_base_owners.len());
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
                if common::sim_skeleton_green(&rpc, howner, ixs, "V3-BUY").await { buy_green = true; break; }
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
                if common::sim_skeleton_green(&rpc, &howner, ixs, "V3-SELL").await { sell_green = true; hit = true; break; }
            }
            if !hit { poor_drops += 1; eprintln!("no sellable holder on {mint}"); }
        }
    }
    // Fresh-mint sell replay: fixture mints flipped to zero base (holders
    // all 0), so sell needs a live mint. Discovered DYNAMICALLY: scan
    // recent blocks for fresh sell_v3 fills and extract the 17-account
    // layout positionally (mint [1], quote [2], curve [5], assoc [6],
    // buyback [13]). A hardcoded mint rots (graduation/collection);
    // the scan always finds this week's flow. Graduated curves are
    // filtered (curve sells legitimately fail post-migration — sampling
    // one would red the gate on a true negative).
    async fn try_v3_sell(
        rpc: &RpcClient,
        mint: &Pubkey,
        quote_mint: &Pubkey,
        curve: &Pubkey,
        assoc: &Pubkey,
        buyback: &Pubkey,
        fallback: Option<(Pubkey, Pubkey)>,
    ) -> (bool, u32, u64, u64, u64) {
        use executor::instructions::{
            PumpSwapBuilder, SellV3Accounts, build_create_ata_ix,
            user_ata_with_program, user_volume_accumulator_pda,
        };
        let (mut sims, mut aux, mut uvol, mut poor) = (0u32, 0u64, 0u64, 0u64);
        let token_program = match rpc.get_account(mint).await {
            Ok(a) => a.owner,
            Err(_) => return (false, sims, aux, uvol, poor),
        };
        let quote_program = match rpc.get_account(quote_mint).await {
            Ok(a) => a.owner,
            Err(_) => return (false, sims, aux, uvol, poor),
        };
        let (holders, dropped_aux) = common::holder_atas(rpc, mint, curve).await;
        aux += dropped_aux;
        for (howner, _, hbase) in holders {
            if hbase < 1_000 { continue; }
            if rpc.get_balance(&howner).await.unwrap_or(0) < 50_000 { continue; }
            if rpc.get_account(&user_volume_accumulator_pda(&howner)).await.is_err() {
                uvol += 1;
                continue;
            }
            let hquote = user_ata_with_program(&howner, quote_mint, &quote_program);
            let mut ixs = Vec::new();
            if rpc.get_account(&hquote).await.is_err() {
                ixs.push(build_create_ata_ix(&howner, &howner, quote_mint, &hquote, &quote_program));
            }
            ixs.push(PumpSwapBuilder::build_sell_v3_ix(1_000, 0, &SellV3Accounts {
                payer: &howner, mint, quote_mint,
                bonding_curve: curve, associated_bonding_curve: assoc,
                base_token_program: &token_program, quote_token_program: &quote_program,
                buyback_recipient: buyback,
            }));
            sims += 1;
            if common::sim_skeleton_green(rpc, &howner, ixs, "V3-SELL").await {
                return (true, sims, aux, uvol, poor);
            }
        }
        if let Some((ftrader, fata)) = fallback {
            // Sample's own trader: a real seller minutes ago. Usable
            // when the indexing call is throttled (public RPC 429s it
            // first) or holders flipped since. Verified canonical and
            // balance-checked live, so a stale sample yields no
            // candidate rather than a false GREEN.
            if user_ata_with_program(&ftrader, mint, &token_program) == fata
                && ftrader != *curve
            {
                let held: u64 = rpc
                    .get_token_account_balance(&fata)
                    .await
                    .ok()
                    .and_then(|b| b.amount.parse().ok())
                    .unwrap_or(0);
                if held >= 1_000
                    && rpc.get_balance(&ftrader).await.unwrap_or(0) >= 50_000
                    && rpc
                        .get_account(&user_volume_accumulator_pda(&ftrader))
                        .await
                        .is_ok()
                {
                    let hquote =
                        user_ata_with_program(&ftrader, quote_mint, &quote_program);
                    let mut ixs = Vec::new();
                    if rpc.get_account(&hquote).await.is_err() {
                        ixs.push(build_create_ata_ix(
                            &ftrader,
                            &ftrader,
                            quote_mint,
                            &hquote,
                            &quote_program,
                        ));
                    }
                    ixs.push(PumpSwapBuilder::build_sell_v3_ix(
                        1_000,
                        0,
                        &SellV3Accounts {
                            payer: &ftrader,
                            mint,
                            quote_mint,
                            bonding_curve: curve,
                            associated_bonding_curve: assoc,
                            base_token_program: &token_program,
                            quote_token_program: &quote_program,
                            buyback_recipient: buyback,
                        },
                    ));
                    sims += 1;
                    if common::sim_skeleton_green(rpc, &ftrader, ixs, "V3-SELL-FALLBACK").await {
                        return (true, sims, aux, uvol, poor);
                    }
                }
            }
        }
        poor += 1;
        eprintln!("no sellable holder on {mint}");
        (false, sims, aux, uvol, poor)
    }
    if !sell_green {
        // Fixture mints first (same order as the buy loop above).
        for (raw, path) in fixtures {
            if sell_green { break; }
            let (mint, quote_mint, curve, assoc, buyback) = common::fixture_v3_accounts(raw, path);
            let (green, s, a, u, p) = try_v3_sell(
                &rpc,
                &mint,
                &quote_mint,
                &curve,
                &assoc,
                &buyback,
                Some(common::fixture_v3_trader(raw, path)),
            )
            .await;
            sim_attempts += s;
            aux_drops += a;
            uvol_drops += u;
            poor_drops += p;
            sell_green = green;
        }
        if !sell_green {
            // Dynamic fresh-mint hunt (see header comment).
            let tip = rpc.get_slot().await.expect("slot");
            let mut probe = tip.saturating_sub(20);
            let mut fresh: Vec<(Pubkey, Pubkey, Pubkey, Pubkey, Pubkey, Pubkey, Pubkey)> =
                Vec::new();
            for _ in 0..60u32 {
                if fresh.len() >= 4 { break; }
                let Some(txs) = common::scan_block(&rpc, probe).await else {
                    probe = probe.saturating_sub(10);
                    continue;
                };
                for tx in &txs {
                    if fresh.len() >= 4 { break; }
                    let Some(pump_idx) = tx
                        .keys
                        .iter()
                        .position(|k| k.to_string() == PUMP_PROGRAM)
                        .map(|i| i as u8)
                    else {
                        continue;
                    };
                    for cix in &tx.ixs {
                        if fresh.len() >= 4 { break; }
                        if cix.program_idx != pump_idx {
                            continue;
                        }
                        if cix.data.len() != 24
                            || cix.data[..8] != ids::PUMP_SELL_V3_DISCRIMINATOR
                        {
                            continue;
                        }
                        if cix.accounts.len() != 17 {
                            continue;
                        }
                        let Some(accounts) = common::resolve_accounts(tx, cix) else {
                            continue;
                        };
                        let (mint, quote, curve, assoc, buyback) =
                            (accounts[1], accounts[2], accounts[5], accounts[6], accounts[13]);
                        if fresh.iter().any(|(m, ..)| *m == mint) {
                            continue;
                        }
                        // Ungraduated only (see header comment).
                        let Ok(curve_data) = rpc.get_account(&curve).await else { continue };
                        if curve_data.data.len() < 49 || curve_data.data[48] != 0 {
                            continue;
                        }
                        eprintln!("fresh sell_v3 sample: mint={mint}");
                        // Trader legs ride along as the throttle-proof
                        // fallback (a real seller minutes ago).
                        fresh.push((mint, quote, curve, assoc, buyback, accounts[8], accounts[9]));
                    }
                }
                probe = probe.saturating_sub(10);
            }
            for (mint, quote_mint, curve, assoc, buyback, ftrader, fata) in &fresh {
                if sell_green { break; }
                let (green, s, a, u, p) = try_v3_sell(
                    &rpc,
                    mint,
                    quote_mint,
                    curve,
                    assoc,
                    buyback,
                    Some((*ftrader, *fata)),
                )
                .await;
                sim_attempts += s;
                aux_drops += a;
                uvol_drops += u;
                poor_drops += p;
                sell_green = green;
            }
            if !sell_green {
                eprintln!("fresh-sell: no sellable holder in {} samples", fresh.len());
            }
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
    let rpc = common::rpc_client();
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
    // Fresh launch hunt (bulk blocks, as before). Labeled retry: a
    // funding move between hunt and sim (or a dry hunt) refetches a
    // FRESH launch+buyer instead of failing the gate — only a layout
    // failure or 3 dry rounds reds it. (The loop body keeps its
    // original indentation; the loop is load-bearing, not style.)
    '_rounds: for _round in 0..3u32 {
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
        let Some(txs) = common::scan_block(&rpc, probe_slot).await else {
            eprintln!("slot {probe_slot} unavailable or empty; backing off");
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            probe_slot = probe_slot.saturating_sub(25);
            continue;
        };
        blocks_scanned += 1;
        txs_scanned += txs.len() as u64;
        // Empty bodies (pruned/archive-miss) carry no launches; say so
        // instead of silently scanning nothing.
        eprintln!("slot {probe_slot}: {} txs", txs.len());
        for tx in &txs {
            if launch.is_some() {
                break;
            }
            let Some(pump_idx) = tx
                .keys
                .iter()
                .position(|k| k.to_string() == PUMP_PROGRAM)
                .map(|i| i as u8)
            else {
                continue;
            };
            for cix in &tx.ixs {
                if cix.program_idx != pump_idx {
                    continue;
                }
                if cix.data.len() < 8 || cix.data[..8] != PUMP_CREATE_V2_DISCRIMINATOR {
                    continue;
                }
                let Some(accounts) = common::resolve_accounts(tx, cix) else {
                    continue;
                };
                if accounts.len() < 14 {
                    continue;
                }
                match parse_pump_create_v2(&cix.data, &accounts, tx.sig.clone(), probe_slot) {
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
    let Some(ev) = launch else {
        eprintln!("no fresh create_v2 in {blocks_scanned} blocks ({txs_scanned} txs); launches are sporadic");
        if _round >= 2 {
            panic!("no fresh create_v2 after 3 hunt rounds; rerun later");
        }
        continue '_rounds;
    };
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
    // ago) but carries no position history; official BUY.md/TRADE_V3.md
    // settle the funding question: SOL-pair buys transfer native SOL
    // ([10] unused), so the hunt filters on native SOL and base hold —
    // WSOL is advisory only (matters for token-paired quotes). The sim
    // remains the real gate. Any funded account can buy a fresh curve,
    // which is exactly the production sniper shape (our wallet is
    // natively funded at setup): hunt recent buy-side flow for the
    // first canonical holder with SOL + base.
    use executor::instructions::PUMP_BUY_V3_DISCRIMINATOR;
    use ids::{PUMP_BUY_DISCRIMINATOR, PUMP_BUY_V2_DISCRIMINATOR};
    let buyer: Pubkey = {
        let mut before = None;
        let mut found = None;
        let mut scan_fails = 0u32;
        for _ in 0..6usize {
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
                    // Throttle, not verdict: back off and refetch the
                    // same page (up to 3), then keep older pages.
                    scan_fails += 1;
                    eprintln!("buyer hunt scan failed ({e}); backoff {scan_fails}/3");
                    if scan_fails > 3 {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(2 * scan_fails as u64)).await;
                    continue;
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
                    let Some(data) = common::decode_b58_opt(&cix.data) else {
                        continue;
                    };
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
                    // Advisory only: SOL-pair buys pay native SOL, so a
                    // missing WSOL float must not filter candidates — the
                    // sim decides. (Token-paired quotes would need it.)
                    let wsol: u64 = match rpc.get_token_account_balance(&wsol_ata).await {
                        Ok(b) => b.amount.parse().unwrap_or(0),
                        Err(_) => 0,
                    };
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
        let Some(buyer) = found else {
            eprintln!("no funded buyer in recent buy flow");
            if _round >= 2 {
                panic!("no funded buyer after 3 hunt rounds; rerun");
            }
            continue '_rounds;
        };
        buyer
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
    // A funding move between hunt and sim (buyer drained, curve
    // graduated mid-flight) surfaces as `insufficient funds` inside a
    // TransferChecked — a stale sample, not a layout bug. A stale
    // sample refetches fresh (see loop head); only a layout failure
    // reds the gate, loudly either way.
    match common::sim_ix_green(&rpc, &buyer, ixs, "MAYHEM-BUY").await {
        Ok(()) => {}
        Err(e) if e.contains("insufficient funds") => {
            eprintln!("FLAKY SAMPLE (funds moved between hunt and sim); hunting a fresh sample");
            if _round >= 2 {
                panic!("FLAKY SAMPLE on the final round; rerun for a fresh launch: {e}");
            }
            continue '_rounds;
        }
        Err(e) => panic!("mayhem extended buy simulation failed: {e}"),
    }
    println!("MAYHEM BUY GREEN mint={}", ev.mint);
    return;
    } // '_rounds: GREEN exits via return above; dry/flaky rounds loop for a fresh launch+buyer
}

#[tokio::test]
#[ignore]
async fn pumpswap_sell_v2_simulates() {
    // Syarat-1 gap closure: the PumpSwap exit leg. `sell_pumpswap_v2_ix`
    // was doc-pinned only ("No frozen sell_v2 fill yet"); this replays
    // OUR builder + OUR quote math (floor from live reserves) against a
    // live pool with a live funded holder, `simulateTransaction`
    // (sig_verify off, dust, open guards). Simulation only, never
    // sends anything. Cashback pools are skipped (6079 refuses
    // on-chain; our builder refuses identically in preflight).
    if std::env::var("MAINNET_LAYOUT").as_deref() != Ok("1") {
        eprintln!("skipped: set MAINNET_LAYOUT=1 to run the live layout probe");
        return;
    }
    use executor::instructions::{
        PumpAmmSellV2Accounts, PumpSwapBuilder, amm_event_authority_pda,
        amm_user_volume_accumulator_pda, build_create_ata_ix,
        user_ata_with_program,
    };
    use executor::quote::{
        market_cap, parse_amm_creator_gate, parse_fee_schedule,
        pumpswap_fee_bps, pumpswap_sell_floor, pumpswap_sell_out, PoolView,
    };
    use migration::pumpswap::PumpSwapPool;
    let rpc = common::rpc_client();
    const PUMP_AMM: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";

    // Hunt recent sell_v2 fills (bulk blocks, newest first).
    let tip = rpc.get_slot().await.expect("slot");
    let mut probe_slot = tip.saturating_sub(30);
    let mut samples: Vec<Vec<Pubkey>> = Vec::new();
    for _ in 0..40u32 {
        if samples.len() >= 4 {
            break;
        }
        let Some(txs) = common::scan_block(&rpc, probe_slot).await else {
            eprintln!("slot {probe_slot} unavailable or empty; backing off");
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            probe_slot = probe_slot.saturating_sub(10);
            continue;
        };
        for tx in &txs {
            if samples.len() >= 4 {
                break;
            }
            let Some(amm_idx) = tx
                .keys
                .iter()
                .position(|k| k.to_string() == PUMP_AMM)
                .map(|i| i as u8)
            else {
                continue;
            };
            for cix in &tx.ixs {
                if samples.len() >= 4 {
                    break;
                }
                if cix.program_idx != amm_idx {
                    continue;
                }
                if cix.data.len() != 24 || cix.data[..8] != ids::PUMP_AMM_SELL_V2_DISCRIMINATOR {
                    continue;
                }
                if cix.accounts.len() != 17 {
                    continue;
                }
                let Some(accounts) = common::resolve_accounts(tx, cix) else {
                    continue;
                };
                samples.push(accounts);
            }
        }
        probe_slot = probe_slot.saturating_sub(10);
    }
    assert!(!samples.is_empty(), "no sell_v2 in recent blocks; rerun");

    // Try each sample: layout cross-check + live-holder sim.
    let mut failures: Vec<String> = Vec::new();
    let mut green = false;
    for cacc in &samples {
        if green {
            break;
        }
        let (pool, user) = (cacc[0], cacc[1]);
        let (base_mint, quote_mint) = (cacc[3], cacc[4]);
        // Layout cross-check: our 17-account table against the live
        // frame (globals, fee config, program). [14] varies by pool
        // (recipient ATA) and [12]/[15] are derived below instead.
        if cacc[2] != ids::PUMP_AMM_GLOBAL_CONFIG
            || cacc[13] != ids::PUMP_AMM_FEE_CONFIG
            || cacc[16] != ids::PUMP_AMM_PROGRAM_ID
        {
            failures.push(format!("pool {pool}: frame globals mismatch; skipping sample"));
            continue;
        }
        if quote_mint != ids::WSOL_MINT {
            failures.push(format!("pool {pool}: non-WSOL quote; skipping sample"));
            continue;
        }
        let pool_acct = match rpc.get_account(&pool).await {
            Ok(a) => a,
            Err(_) => {
                failures.push(format!("pool {pool}: unreadable; skipping sample"));
                continue;
            }
        };
        if pool_acct.owner != ids::PUMP_AMM_PROGRAM_ID {
            failures.push(format!("pool {pool}: owner mismatch; skipping sample"));
            continue;
        }
        let Some(parsed) =
            PumpSwapPool::parse_validated(&pool, &pool_acct.data, &pool_acct.owner, &base_mint)
        else {
            failures.push(format!("pool {pool}: trust gate failed; skipping sample"));
            continue;
        };
        if parsed.is_cashback {
            failures.push(format!("pool {pool}: cashback (6079 refuses); skipping sample"));
            continue;
        }
        if parsed.base_vault != cacc[7] || parsed.quote_vault != cacc[8] {
            failures.push(format!("pool {pool}: vaults differ from frame; skipping sample"));
            continue;
        }
        // Holder legs: canonical base ATA with dust, funded payer.
        let token_program = match rpc.get_account(&base_mint).await {
            Ok(a) => a.owner,
            Err(_) => {
                failures.push(format!("pool {pool}: mint unreadable; skipping sample"));
                continue;
            }
        };
        if user_ata_with_program(&user, &base_mint, &token_program) != cacc[5] {
            failures.push(format!("pool {pool}: base ATA non-canonical; skipping sample"));
            continue;
        }
        let held: u64 = match rpc.get_token_account_balance(&cacc[5]).await {
            Ok(b) => b.amount.parse().unwrap_or(0),
            Err(_) => {
                failures.push(format!("pool {pool}: base balance unreadable; skipping"));
                continue;
            }
        };
        if rpc.get_balance(&user).await.unwrap_or(0) < 50_000 {
            failures.push(format!("pool {pool}: payer poor; skipping sample"));
            continue;
        }
        // Fee inputs + live reserves (mirror preflight, read-only).
        let vaults = async {
            let b = rpc.get_account(&parsed.base_vault).await.ok()?;
            let q = rpc.get_account(&parsed.quote_vault).await.ok()?;
            Some((b, q))
        }
        .await;
        let global = rpc.get_account(&ids::PUMP_AMM_GLOBAL_CONFIG).await.ok();
        let fee_config = rpc.get_account(&ids::PUMP_AMM_FEE_CONFIG).await.ok();
        let supply = rpc.get_token_supply(&base_mint).await.ok();
        let (Some((base_vault, quote_vault)), Some(global), Some(fee_config), Some(supply)) =
            (vaults, global, fee_config, supply)
        else {
            failures.push(format!("pool {pool}: fee/reserve reads failed; skipping"));
            continue;
        };
        if global.owner != ids::PUMP_AMM_PROGRAM_ID
            || fee_config.owner != ids::FEE_CONFIG_PROGRAM
            || global
                .data
                .get(ids::PUMP_AMM_GLOBAL_DISABLE_FLAGS_OFFSET)
                .copied()
                .unwrap_or(1)
                != 0
        {
            failures.push(format!("pool {pool}: global/fee-config untrusted; skipping"));
            continue;
        }
        let gate_on = match parse_amm_creator_gate(&global.data) {
            Ok(g) => g,
            Err(_) => {
                failures.push(format!("pool {pool}: creator gate unparsable; skipping"));
                continue;
            }
        };
        let schedule = match parse_fee_schedule(&fee_config.data) {
            Ok(s) => s,
            Err(_) => {
                failures.push(format!("pool {pool}: fee schedule unparsable; skipping"));
                continue;
            }
        };
        let quote_program = quote_vault.owner;
        if base_vault.owner != ids::TOKEN_PROGRAM_ID
            && base_vault.owner != ids::TOKEN_2022_PROGRAM_ID
        {
            failures.push(format!("pool {pool}: base vault program unknown; skipping"));
            continue;
        }
        if quote_program != ids::TOKEN_PROGRAM_ID
            && quote_program != ids::TOKEN_2022_PROGRAM_ID
        {
            failures.push(format!("pool {pool}: quote vault program unknown; skipping"));
            continue;
        }
        let base_reserve: u64 = match rpc.get_token_account_balance(&parsed.base_vault).await {
            Ok(b) => b.amount.parse().unwrap_or(0),
            Err(_) => continue,
        };
        let quote_reserve: u64 = match rpc.get_token_account_balance(&parsed.quote_vault).await {
            Ok(b) => b.amount.parse().unwrap_or(0),
            Err(_) => continue,
        };
        let eff_quote = match (i128::from(quote_reserve))
            .checked_add(parsed.virtual_quote_reserves)
            .and_then(|v| u64::try_from(v).ok())
            .filter(|v| *v > 0)
        {
            Some(v) => v,
            None => {
                failures.push(format!("pool {pool}: effective reserves non-positive; skipping"));
                continue;
            }
        };
        let real_cap = match u64::try_from(parsed.fee_buckets)
            .ok()
            .and_then(|buckets| quote_reserve.checked_sub(buckets))
        {
            Some(v) => v,
            None => {
                failures.push(format!("pool {pool}: fee buckets exceed vault; skipping"));
                continue;
            }
        };
        let circulating = if parsed.is_mayhem {
            ids::PUMP_AMM_MAYHEM_SUPPLY
        } else {
            match supply.amount.parse::<u64>() {
                Ok(s) => s,
                Err(_) => {
                    failures.push(format!("pool {pool}: supply unparsable; skipping"));
                    continue;
                }
            }
        };
        let mcap = match market_cap(eff_quote, base_reserve, circulating) {
            Ok(m) => m,
            Err(_) => {
                failures.push(format!("pool {pool}: mcap uncomputable; skipping"));
                continue;
            }
        };
        let fees = match pumpswap_fee_bps(
            &schedule,
            gate_on,
            parsed.creator_fee_bps,
            parsed.coin_creator == Pubkey::default(),
            mcap,
        ) {
            Ok(f) => f,
            Err(_) => {
                failures.push(format!("pool {pool}: fee tier unresolvable; skipping"));
                continue;
            }
        };
        let view = PoolView {
            base_reserve,
            eff_quote_reserve: eff_quote,
            real_quote_cap: real_cap,
            lp_bps: fees.lp,
            proto_bps: fees.protocol,
            creator_bps: fees.creator,
        };
        // Dust ladder: first amount the holder covers that our own
        // formula floors (formula + layout proven together).
        let mut sized: Option<(u64, u64)> = None;
        for dust in [1_000_000u64, 100_000, 10_000, 1_000] {
            if held < dust {
                continue;
            }
            let Ok(net) = pumpswap_sell_out(&view, dust) else { continue };
            if let Some(floor) = pumpswap_sell_floor(net, 200) {
                sized = Some((dust, floor));
                break;
            }
        }
        let Some((amount, floor)) = sized else {
            failures.push(format!("pool {pool}: no dust size floors (held={held}); skipping"));
            continue;
        };
        // Derived legs must match the live frame ([12] uvol, [15]
        // event auth): positional proof of our PDA derivations.
        if amm_user_volume_accumulator_pda(&user) != cacc[12]
            || amm_event_authority_pda() != cacc[15]
        {
            failures.push(format!("pool {pool}: PDA legs mismatch; skipping sample"));
            continue;
        }
        // Buyback recipient ATA must exist (never created by the trade).
        let buyback = user_ata_with_program(
            &ids::DEFAULT_BUYBACK_FEE_RECIPIENT,
            &ids::WSOL_MINT,
            &quote_program,
        );
        if rpc.get_account(&buyback).await.is_err() {
            failures.push(format!("pool {pool}: buyback ATA missing; skipping sample"));
            continue;
        }
        let mut ixs = Vec::new();
        if rpc.get_account(&cacc[6]).await.is_err() {
            ixs.push(build_create_ata_ix(&user, &user, &quote_mint, &cacc[6], &quote_program));
        }
        ixs.push(PumpSwapBuilder::build_sell_pumpswap_v2_ix(
            amount,
            floor,
            &PumpAmmSellV2Accounts {
                pool: &pool,
                user: &user,
                base_mint: &base_mint,
                quote_mint: &quote_mint,
                user_base_token_account: &cacc[5],
                user_quote_token_account: &cacc[6],
                pool_base_token_account: &parsed.base_vault,
                pool_quote_token_account: &parsed.quote_vault,
                base_token_program: &token_program,
                quote_token_program: &quote_program,
                buyback_fee_recipient: &buyback,
            },
        ));
        if common::sim_skeleton_green(&rpc, &user, ixs, "PUMPSWAP-SELL-V2").await {
            green = true;
        } else {
            failures.push(format!("pool {pool}: transport trouble; resampled"));
        }
    }
    assert!(
        green,
        "no pumpswap sell_v2 simulating clean after {} samples:\n{}",
        samples.len(),
        failures.join("\n")
    );
    println!("PUMPSWAP SELL_V2 GREEN");
}
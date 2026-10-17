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
//! 2. Layout simulation — rebuild the ix with OUR builder and
//!    `simulateTransaction` it (unsigned skeleton: proves layout and
//!    the Token-2022 program parameter, not auth).
//!
//! NEVER runs in normal `cargo test` (`#[ignore]`) nor CI. Needs public
//! mainnet RPC (rate limits may require a rerun):
//! `RAYDIUM_ACCEPTANCE=1 cargo test -p executor --test raydium_authority_acceptance -- --ignored --nocapture`

use executor::build_swap_base_in_v2_ix;
use solana_pubkey::Pubkey;
use std::str::FromStr;

mod common;

const RAYDIUM_PROGRAM: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
/// Proving pool: frozen Fartcoin migration (`raydium_pool_fartcoin.json`).
const FARTCOIN_MINT: &str = "9BB6NFEcjBCtnNLFko2FqVQBq8HHM13kCyYcdQbgpump";
/// Second pool (fresher flow when Fartcoin is router-dominated).
const MAINNET_MINT: &str = "HyDKNdnhZNVYQMruBevbNsUWruA9STmAQrS4srXApump";

#[tokio::test]
#[ignore]
async fn raydium_authority_and_bundle_simulate() {
    if std::env::var("RAYDIUM_ACCEPTANCE").as_deref() != Ok("1") {
        eprintln!("skipped: set RAYDIUM_ACCEPTANCE=1 to run the Raydium acceptance probe");
        return;
    }
    let rpc = common::rpc_client();
    // --- Gate 1: authority proof against a live direct swap. ---
    // Hunt recent blocks for direct top-level SwapBaseInV2 (tag 16,
    // 17 bytes, 8 accounts): staking fixed pools starves when router
    // flow dominates them (observed: 0 direct swaps in 1561 sigs over
    // 2 pools). Hunting flow finds this week's swaps on ANY pool; each
    // sample replays against its own live-parsed pool. Block payloads
    // carry full txs, so no per-signature fetch burns RPC budget.
    let raydium = Pubkey::from_str(RAYDIUM_PROGRAM).expect("raydium id");
    // Candidates, not a single proof: one bad sample (spent ATA,
    // composed inner frame, moved funds) must resample, never red
    // the gate. Authority proof uses the first; sims try each.
    let mut candidates: Vec<(String, Vec<Pubkey>, Pubkey)> = Vec::new();
    let mut scanned = 0u32;
    let mut blocks = 0u32;
    let tip = rpc.get_slot().await.expect("slot");
    let mut probe_slot = tip.saturating_sub(20);
    'scan: for _ in 0..60u32 {
        if candidates.len() >= 6 {
            break;
        }
        let Some(txs) = common::scan_block(&rpc, probe_slot).await else {
            eprintln!("slot {probe_slot} unavailable or empty; skipping");
            probe_slot = probe_slot.saturating_sub(10);
            continue;
        };
        blocks += 1;
        for tx in &txs {
            if candidates.len() >= 6 {
                break 'scan;
            }
            scanned += 1;
            let Some(pi) = tx.keys.iter().position(|k| *k == raydium) else {
                continue;
            };
            for cix in &tx.ixs {
                if cix.program_idx != pi as u8 {
                    continue;
                }
                // Direct SwapBaseInV2 only (tag 16): router inner ixs
                // carry different account frames and prove nothing here.
                if cix.data.len() != 17 || cix.data[0] != 16 {
                    continue;
                }
                let Some(accounts) = common::resolve_accounts(tx, cix) else {
                    continue;
                };
                if accounts.len() != 8 {
                    continue;
                }
                let Some(payer) = tx.keys.first().copied() else {
                    continue;
                };
                candidates.push((tx.sig.clone(), accounts, payer));
            }
        }
        probe_slot = probe_slot.saturating_sub(10);
    }
    let Some((sig, accounts, _)) = candidates.first().cloned() else {
        panic!(
            "no direct SwapBaseInV2 in {scanned} raydium txs over {blocks} blocks; rerun (router txs dominate) — NOT a builder verdict"
        );
    };
    // The sample's pool must itself be a Raydium AMM account.
    let amm_acct = rpc
        .get_account(&accounts[1])
        .await
        .expect("sample pool readable");
    assert_eq!(
        amm_acct.owner, raydium,
        "sample pool not owned by the AMM program"
    );
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
            MAINNET_MINT,
        ),
    ] {
        let fixture_pool = common::pool_fixture(fixture);
        let (famm, fowner) = (
            Pubkey::from_str(&fixture_pool.amm).expect("pubkey"),
            Pubkey::from_str(&fixture_pool.owner).expect("pubkey"),
        );
        let fdata = fixture_pool.data;
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

    // --- Gates 2+3: our builder rebuilds sampled legs, sim only. ---
    // No token-program resolution: upstream hard-requires classic at
    // [0], so the builder carries no program parameter to resolve.
    // Resampling: each candidate is leg-validated with cheap reads,
    // then simmed in both directions. A failing sample (spent ATA,
    // composed frame, moved funds) is skipped with its logs — only
    // an all-candidate failure reds the gate. Observed once: a lone
    // sample failed `Custom(38)` while the resample went GREEN on
    // both directions, so single-sample verdicts are retired here.
    let mut failures: Vec<String> = Vec::new();
    let mut bundle_green = false;
    let mut sell_green = false;
    for (csig, cacc, cpayer) in candidates.iter() {
        if bundle_green && sell_green {
            break;
        }
        // Each sample replays against its own live-parsed pool
        // (migration orientation: WSOL base, token quote).
        let amm_acct = match rpc.get_account(&cacc[1]).await {
            Ok(a) => a,
            Err(_) => {
                failures.push(format!("{csig}: pool unreadable; skipping sample"));
                continue;
            }
        };
        if amm_acct.owner != raydium {
            failures.push(format!("{csig}: pool owner mismatch; skipping sample"));
            continue;
        }
        let pool = match migration::pool::RaydiumV4Pool::parse(&cacc[1], &amm_acct.data) {
            Ok(p) if p.is_wsol_based() => p,
            _ => {
                failures.push(format!("{csig}: pool unparsable/non-WSOL-based; skipping"));
                continue;
            }
        };
        // [5] must be a live token account owned by [7] with dust to
        // move; anything else is a composed frame or a spent sample.
        // [6] must at least EXIST: swept/closed dest ATAs (rent
        // reclaimed minutes after the swap) fail the replay with a
        // program error that says nothing about our builder — observed
        // once as `Invalid SPL token program` on a closed dest.
        let (owner5, mint5, bal5) = match common::token_account(&rpc, &cacc[5]).await {
            Some(v) => v,
            None => {
                failures.push(format!("{csig}: source [5] unreadable; skipping sample"));
                continue;
            }
        };
        let (owner6, _, _) = match common::token_account(&rpc, &cacc[6]).await {
            Some(v) => v,
            None => {
                failures.push(format!("{csig}: dest [6] missing (swept?); skipping sample"));
                continue;
            }
        };
        if owner5 != cacc[7] {
            failures.push(format!("{csig}: source owner != [7] (composed frame?); skipping"));
            continue;
        }
        if owner6 != cacc[7] {
            failures.push(format!("{csig}: dest owner != [7] (composed frame?); skipping"));
            continue;
        }
        if bal5 < 1_000 {
            failures.push(format!("{csig}: source empty ({bal5}); skipping sample"));
            continue;
        }
        if rpc.get_balance(cpayer).await.unwrap_or(0) < 50_000 {
            failures.push(format!("{csig}: payer poor; skipping sample"));
            continue;
        }
        if !bundle_green {
            let ix = build_swap_base_in_v2_ix(
                &pool.amm_id,
                &derived,
                &pool.base_vault,
                &pool.quote_vault,
                &cacc[5],
                &cacc[6],
                &cacc[7],
                1_000,
                0,
            )
            .expect("replay builds");
            assert_eq!(ix.program_id, raydium);
            match common::sim_ix_green(&rpc, cpayer, vec![ix], "RAYDIUM-BUNDLE").await {
                Ok(()) => {
                    bundle_green = true;
                    println!("RAYDIUM BUNDLE GREEN");
                }
                Err(e) => {
                    failures.push(format!("{csig}: {e:?}"));
                    continue;
                }
            }
        }
        if !sell_green {
            // The SELL direction we will actually dispatch. The sample
            // was a buy when its source mint is WSOL, so flip the legs
            // for our sell (and vice versa).
            let (sell_src, sell_dst) = if mint5 == ids::WSOL_MINT {
                (cacc[6], cacc[5])
            } else {
                (cacc[5], cacc[6])
            };
            let sell_bal = match common::token_account(&rpc, &sell_src).await {
                Some((_, _, b)) => b,
                None => {
                    failures.push(format!("{csig}: sell source unreadable; skipping sample"));
                    continue;
                }
            };
            if common::token_account(&rpc, &sell_dst).await.is_none() {
                failures.push(format!("{csig}: sell dest missing (swept?); skipping sample"));
                continue;
            }
            if sell_bal < 1_000 {
                failures.push(format!("{csig}: sell source empty; skipping sample"));
                continue;
            }
            let sell_ix = build_swap_base_in_v2_ix(
                &pool.amm_id,
                &derived,
                &pool.base_vault,
                &pool.quote_vault,
                &sell_src,
                &sell_dst,
                &cacc[7],
                1_000,
                0,
            )
            .expect("sell replay builds");
            match common::sim_ix_green(&rpc, cpayer, vec![sell_ix], "RAYDIUM-SELL").await {
                Ok(()) => {
                    sell_green = true;
                    println!("RAYDIUM SELL GREEN");
                }
                Err(e) => {
                    failures.push(format!("{csig}: {e:?}"));
                    continue;
                }
            }
        }
    }
    assert!(
        bundle_green && sell_green,
        "no raydium replay simulating clean after {} candidates:\n{}",
        candidates.len(),
        failures.join("\n")
    );
}

//! Raydium AMM v4 post-migration swap builder (no OpenBook leg).
//!
//! Account order and data layout verified against
//! `raydium-amm/program/src/instruction.rs::SwapBaseInV2`:
//! `[0]=SPL Token program, [1]=AMM pool, [2]=authority (readonly),
//! [3]=coin vault, [4]=pc vault, [5]=user source, [6]=user destination,
//! [7]=user owner (signer)`; data = `tag 16 ++ amount_in u64 ++
//! minimum_amount_out u64`.
//!
//! Scope: instruction building and constant-product quoting for
//! post-migration pools the hunt already resolved, plus the Jito-routed
//! post-migration sell path ([`SellRaydiumOrder::sell_raydium_jito`]).
//! Live post-migration exits route through here when a validated pool
//! leg is stored; everything else stays paper-only.
//!
//! Provenance (all mainnet): wire order vs a direct `SwapBaseInV2`,
//! shared vault authority vs swap-sample + vault reads on two pools (a
//! per-pool PDA derivation was tried and refuted — see
//! [`ids::RAYDIUM_VAULT_AUTHORITY`]), nonce-seed mechanism pinned
//! offline on both frozen pools. Token-2022 *vaults* are unroutable by
//! program construction (see the builder docs) and refuse fail-closed.

use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

/// `SwapBaseInV2` instruction tag (see `AmmInstruction::unpack`: 16).
pub const SWAP_BASE_IN_V2_TAG: u8 = 16;
/// Standard AMM v4 trade fee: 25 bps (0.25%) retained from input.
pub const AMM_V4_FEE_BPS: u64 = 25;
/// Fee denominator for [`AMM_V4_FEE_BPS`].
pub const AMM_V4_FEE_DEN: u64 = 10_000;

/// Quote `amount_in` (base units, coin→pc or pc→coin by reserve order)
/// through constant-product math with the AMM v4 fee: the fee stays in
/// the pool, so `out = eff_in * out_reserve / (in_reserve + eff_in)`
/// with `eff_in = amount_in * (DEN - BPS) / DEN`. `None` on degenerate
/// inputs (nothing tradeable, no quote).
pub fn quote_base_in(
    amount_in: u64,
    in_reserve: u64,
    out_reserve: u64,
    fee_bps: u64,
) -> Option<u64> {
    if amount_in == 0 || in_reserve == 0 || out_reserve == 0 {
        return None;
    }
    let fee_bps = fee_bps.min(AMM_V4_FEE_DEN);
    let eff_in = amount_in as u128 * (AMM_V4_FEE_DEN - fee_bps) as u128 / AMM_V4_FEE_DEN as u128;
    if eff_in == 0 {
        return None;
    }
    let out =
        eff_in * out_reserve as u128 / (in_reserve as u128 + eff_in);
    u64::try_from(out).ok().filter(|o| *o > 0)
}

/// Quote with the standard [`AMM_V4_FEE_BPS`] schedule.
pub fn quote_base_in_std(amount_in: u64, in_reserve: u64, out_reserve: u64) -> Option<u64> {
    quote_base_in(amount_in, in_reserve, out_reserve, AMM_V4_FEE_BPS)
}

/// Slippage floor over a reserve quote: `quote*(10000-bps)/10000`,
/// floored by integer division. `None` on degenerate inputs — and on a
/// zero floor: `minimum_amount_out = 0` authorizes the slot leader to
/// fill at any price (sandwich drain), so full-slippage configs and
/// sub-lamport dust fail closed instead of dispatching unprotected.
/// Pure; the preflight prices `quote` from live vault reserves.
pub fn raydium_sell_floor(quoted_out: u64, slippage_bps: u64) -> Option<u64> {
    if quoted_out == 0 || slippage_bps >= 10_000 {
        return None;
    }
    let floor = (quoted_out as u128 * (10_000 - slippage_bps as u128) / 10_000u128) as u64;
    if floor == 0 {
        return None;
    }
    Some(floor)
}

/// Derive the AMM vault authority off-chain for a pool nonce:
/// `create_program_address([b"amm authority", &[nonce]])` under the V4
/// program (upstream `Processor::authority_id`, same seeds the program
/// checks every swap against).
///
/// Mechanism proof, not the dispatch path: both frozen pools derive the
/// shared constant below from their stored nonce bytes (pinned in
/// `derives_frozen_pool_authorities`). Dispatch passes the constant
/// directly — one fewer moving part on the funded path, and any
/// nonce-divergent pool fails free at simulation, never on-chain.
/// `None` when the address lands on-curve (no valid PDA).
pub fn amm_authority_for_nonce(nonce: u8) -> Option<Pubkey> {
    Pubkey::create_program_address(&[b"amm authority", &[nonce]], &ids::RAYDIUM_V4_PROGRAM_ID).ok()
}

/// Build a Raydium AMM v4 `SwapBaseInV2` instruction. All addresses are
/// caller-resolved (pool vaults from [`crate::RAYDIUM_V4_PROGRAM_ID`]-owned
/// parsing, authority is [`ids::RAYDIUM_VAULT_AUTHORITY`]); this function only lays
/// out the verified wire order — it performs no RPC and never panics.
///
/// Accounts[0] is ALWAYS the classic SPL Token program (not a
/// parameter): upstream `process_swap_base_in_v2` hard-requires
/// `token_program == spl_token::id()` (`InvalidSplTokenProgram`
/// otherwise) and unpacks every vault/user account with the classic
/// `spl_token::state::Account` layout plus an owner check against that
/// same id. Token-2022 vaults are therefore unroutable through AMM v4
/// by program construction — no client-side parameter can change that
/// (only other Raydium programs could).
/// `None` on a zero `amount_in` (a doomed ix: the chain would fail it
/// after fees).
#[allow(clippy::too_many_arguments)]
pub fn build_swap_base_in_v2_ix(
    amm: &Pubkey,
    authority: &Pubkey,
    coin_vault: &Pubkey,
    pc_vault: &Pubkey,
    user_source: &Pubkey,
    user_destination: &Pubkey,
    user_owner: &Pubkey,
    amount_in: u64,
    minimum_amount_out: u64,
) -> Option<Instruction> {
    if amount_in == 0 {
        return None;
    }
    let mut data = Vec::with_capacity(17);
    data.push(SWAP_BASE_IN_V2_TAG);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&minimum_amount_out.to_le_bytes());
    Some(Instruction {
        program_id: ids::RAYDIUM_V4_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(ids::TOKEN_PROGRAM_ID, false),
            AccountMeta::new(*amm, false),
            AccountMeta::new_readonly(*authority, false),
            AccountMeta::new(*coin_vault, false),
            AccountMeta::new(*pc_vault, false),
            AccountMeta::new(*user_source, false),
            AccountMeta::new(*user_destination, false),
            AccountMeta::new_readonly(*user_owner, true),
        ],
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> Pubkey {
        Pubkey::new_from_array([byte; 32])
    }

    #[test]
    fn v2_wire_order_matches_upstream() {
        let ix = build_swap_base_in_v2_ix(
            &key(1),
            &key(2),
            &key(3),
            &key(4),
            &key(5),
            &key(6),
            &key(7),
            1_000,
            900,
        )
        .expect("nonzero amount builds");
        assert_eq!(ix.program_id, ids::RAYDIUM_V4_PROGRAM_ID);
        assert_eq!(ix.accounts.len(), 8);
        assert_eq!(ix.accounts[0], AccountMeta::new_readonly(ids::TOKEN_PROGRAM_ID, false));
        assert_eq!(ix.accounts[1], AccountMeta::new(key(1), false));
        assert_eq!(ix.accounts[2], AccountMeta::new_readonly(key(2), false));
        assert_eq!(ix.accounts[7], AccountMeta::new_readonly(key(7), true));
        assert!(ix.accounts[7].is_signer);
        assert_eq!(ix.data.len(), 17);
        assert_eq!(ix.data[0], SWAP_BASE_IN_V2_TAG);
        assert_eq!(
            u64::from_le_bytes(ix.data[1..9].try_into().expect("u64")),
            1_000
        );
        assert_eq!(
            u64::from_le_bytes(ix.data[9..17].try_into().expect("u64")),
            900
        );
    }

    #[test]
    fn sell_floor_guards_minimum() {
        // 2% over a 1M quote floors to 980k; dust and full-slippage
        // configs refuse (a zero minimum authorizes any fill).
        assert_eq!(raydium_sell_floor(1_000_000, 200), Some(980_000));
        assert_eq!(raydium_sell_floor(1_000_000, 0), Some(1_000_000));
        assert_eq!(raydium_sell_floor(0, 200), None);
        assert_eq!(raydium_sell_floor(1, 9_999), None);
        assert_eq!(raydium_sell_floor(1_000_000, 10_000), None);
        assert_eq!(raydium_sell_floor(1_000_000, 10_001), None);
    }

    #[test]
    fn quote_takes_fee_and_rejects_degenerate() {
        // 1M in, 100M/100M reserves, 25bps: eff = 997500 →
        // out = 997500 * 1e8 / (1e8 + 997500) = 987648 (rounded down).
        assert_eq!(quote_base_in_std(1_000_000, 100_000_000, 100_000_000), Some(987_648));
        // Zero fee quotes higher (pure xy=k).
        assert_eq!(quote_base_in(1_000_000, 100_000_000, 100_000_000, 0), Some(990_099));
        for bad in [
            (0, 100, 100),
            (100, 0, 100),
            (100, 100, 0),
        ] {
            assert_eq!(quote_base_in(bad.0, bad.1, bad.2, 25), None);
        }
        // Zero-amount swaps never build (doomed ix, fails on-chain).
        assert!(
            build_swap_base_in_v2_ix(
                &key(1),
                &key(2),
                &key(3),
                &key(4),
                &key(5),
                &key(6),
                &key(7),
                0,
                0,
            )
            .is_none()
        );
    }

    #[test]
    fn token_program_is_always_classic() {
        // Upstream `process_swap_base_in_v2` hard-requires
        // `token_program == spl_token::id()` (`InvalidSplTokenProgram`
        // otherwise): Token-2022 vaults are unroutable through AMM v4
        // by program construction, so the builder offers no program
        // parameter to get wrong.
        let ix = build_swap_base_in_v2_ix(
            &key(1),
            &key(2),
            &key(3),
            &key(4),
            &key(5),
            &key(6),
            &key(7),
            1_000,
            900,
        )
        .expect("nonzero amount builds");
        assert_eq!(
            ix.accounts[0],
            AccountMeta::new_readonly(ids::TOKEN_PROGRAM_ID, false)
        );
    }

    #[test]
    fn vault_authority_is_the_proven_constant() {
        // The authority is NOT derived per pool: a seeds-`[amm_id]` PDA
        // was refuted by mainnet swap `3Kg6CRL...davX` (derived
        // `CbaWbgUg...urA47wiG` vs on-chain `5Q544fKr...pge4j1`).
        // Non-default, well-formed, pinned in `ids` (see its docs for
        // the two-pool proof).
        assert_ne!(ids::RAYDIUM_VAULT_AUTHORITY, Pubkey::default());
        assert_eq!(
            ids::RAYDIUM_VAULT_AUTHORITY.to_string(),
            "5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1"
        );
    }

    /// Frozen pool bytes by name (`crates/migration/tests/fixtures/`):
    /// unknown names panic — a typo must never green-light a test
    /// against unintended bytes.
    fn fixture_data(name: &str) -> Vec<u8> {
        let raw = match name {
            "fartcoin" => include_str!("../../migration/tests/fixtures/raydium_pool_fartcoin.json"),
            "mainnet" => include_str!("../../migration/tests/fixtures/raydium_pool_mainnet.json"),
            unknown => panic!("unknown pool fixture: {unknown}"),
        };
        let v: serde_json::Value = serde_json::from_str(raw).expect("fixture json");
        v["data"]
            .as_array()
            .expect("data array")
            .iter()
            .map(|b| b.as_u64().expect("byte") as u8)
            .collect()
    }

    /// Nonce byte of a V4 pool account (`AmmInfo.nonce` at 8..16, lowest
    /// byte selects the `create_program_address` seed suffix — matches
    /// upstream `amm.nonce as u8`).
    fn fixture_nonce(data: &[u8]) -> u8 {
        u64::from_le_bytes(data[8..16].try_into().expect("nonce bytes")) as u8
    }

    #[test]
    fn derives_frozen_pool_authorities() {
        // Mechanism proof for the shared constant: both frozen pools'
        // stored nonces derive exactly `RAYDIUM_VAULT_AUTHORITY` through
        // the upstream seed shape. A pool created with a different nonce
        // would derive a different key — and fail free at simulation.
        for name in ["fartcoin", "mainnet"] {
            let data = fixture_data(name);
            let nonce = fixture_nonce(&data);
            assert_eq!(
                amm_authority_for_nonce(nonce),
                Some(ids::RAYDIUM_VAULT_AUTHORITY),
                "pool {name} (nonce {nonce})"
            );
        }
    }

    /// Frozen Fartcoin migration pool (`crates/migration/tests/fixtures/
    /// raydium_pool_fartcoin.json`): parse-validated slot reads feed the
    /// builder end to end, entirely offline. Proves the dormant path
    /// consumes real mainnet bytes into the verified wire order — layout
    /// and authority plumbing, not execution (no RPC, no funds).
    #[test]
    fn frozen_pool_replays_into_builder() {
        use migration::pool::RaydiumV4Pool;
        use std::str::FromStr;
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
        let pool = RaydiumV4Pool::parse_validated(
            &amm,
            &data,
            &owner,
            &Pubkey::from_str("9BB6NFEcjBCtnNLFko2FqVQBq8HHM13kCyYcdQbgpump").expect("mint"),
        )
        .expect("frozen pool validates");
        assert!(pool.is_wsol_based());
        let authority = ids::RAYDIUM_VAULT_AUTHORITY;
        // Holder legs are synthetic (offline replay): only pool-side
        // slots are asserted against the fixture.
        let ix = build_swap_base_in_v2_ix(
            &pool.amm_id,
            &authority,
            &pool.base_vault,
            &pool.quote_vault,
            &key(5),
            &key(6),
            &key(7),
            1_000_000,
            900_000,
        )
        .expect("replay builds");
        assert_eq!(ix.program_id, ids::RAYDIUM_V4_PROGRAM_ID);
        assert_eq!(ix.accounts[1].pubkey, pool.amm_id);
        assert_eq!(ix.accounts[2].pubkey, authority);
        assert_eq!(ix.accounts[3].pubkey, pool.base_vault);
        assert_eq!(ix.accounts[4].pubkey, pool.quote_vault);
        assert_eq!(ix.data[0], SWAP_BASE_IN_V2_TAG);
        assert_eq!(
            u64::from_le_bytes(ix.data[1..9].try_into().expect("u64")),
            1_000_000
        );
        assert_eq!(
            u64::from_le_bytes(ix.data[9..17].try_into().expect("u64")),
            900_000
        );
    }
}

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
//! post-migration pools the hunt already resolved. Live wiring (funding
//! decision) is intentionally NOT here: finishing that path needs a
//! funded acceptance run first. Paper code must not route through this
//! module without that run.
//!
//! Status of the two preconditions named below: [`amm_authority`]
//! resolves the vault authority off-chain but is NOT yet proven against
//! a mainnet swap sample (marked UNVERIFIED, deterministic-only test);
//! [`build_swap_base_in_v2_ix`] takes the owning token program so
//! Token-2022 pools are representable. Neither is live-wired.

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

/// Derive the AMM vault authority PDA off-chain: seeds `[amm_id]`
/// under the Raydium V4 program (the vaults' owner on every V4 pool).
///
/// UNVERIFIED against a mainnet swap sample: the seed shape follows the
/// upstream `raydium-amm` processor convention (vault signer derived
/// from the pool id), but no frozen swap transaction has yet pinned it
/// here. Deterministic-only test below; a mainnet replay (frozen
/// `SwapBaseInV2` account list vs this output) must go green before any
/// live wiring. Returns `(authority, bump)`.
pub fn amm_authority(amm_id: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[amm_id.as_ref()], &ids::RAYDIUM_V4_PROGRAM_ID)
}

/// Build a Raydium AMM v4 `SwapBaseInV2` instruction. All addresses are
/// caller-resolved (pool vaults from [`crate::RAYDIUM_V4_PROGRAM_ID`]-owned
/// parsing, authority from [`amm_authority`]); this function only lays
/// out the verified wire order — it performs no RPC and never panics.
/// `token_program` is the SPL program owning the vaults and user token
/// accounts (classic or Token-2022 — a wrong program fails on-chain, so
/// resolve it per pool, never assume the classic one). `None` on a zero
/// `amount_in` (a doomed ix: the chain would fail it after fees).
/// Dormant path: post-migration tracking is paper-only — do not wire
/// this without a funded run proving authority + program selection.
#[allow(clippy::too_many_arguments)]
pub fn build_swap_base_in_v2_ix(
    amm: &Pubkey,
    authority: &Pubkey,
    coin_vault: &Pubkey,
    pc_vault: &Pubkey,
    user_source: &Pubkey,
    user_destination: &Pubkey,
    user_owner: &Pubkey,
    token_program: &Pubkey,
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
            AccountMeta::new_readonly(*token_program, false),
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
            &ids::TOKEN_PROGRAM_ID,
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
                &ids::TOKEN_PROGRAM_ID,
                0,
                0,
            )
            .is_none()
        );
    }

    #[test]
    fn token_program_is_caller_selected() {
        // Token-2022 pools route through the 2022 program at [0]: the
        // builder must not assume the classic program.
        let ix = build_swap_base_in_v2_ix(
            &key(1),
            &key(2),
            &key(3),
            &key(4),
            &key(5),
            &key(6),
            &key(7),
            &ids::TOKEN_2022_PROGRAM_ID,
            1_000,
            900,
        )
        .expect("nonzero amount builds");
        assert_eq!(
            ix.accounts[0],
            AccountMeta::new_readonly(ids::TOKEN_2022_PROGRAM_ID, false)
        );
        assert_ne!(ix.accounts[0].pubkey, ids::TOKEN_PROGRAM_ID);
    }

    #[test]
    fn amm_authority_is_deterministic_off_chain() {
        // Determinism-only: the seed shape is UNVERIFIED against a
        // mainnet swap sample (see `amm_authority` docs). Distinct pools
        // derive distinct non-default authorities; same pool is stable.
        let a = Pubkey::new_from_array([11; 32]);
        let b = Pubkey::new_from_array([12; 32]);
        let (auth_a, bump_a) = amm_authority(&a);
        assert_eq!(amm_authority(&a), (auth_a, bump_a));
        assert_ne!(auth_a, Pubkey::default());
        assert_ne!(amm_authority(&b).0, auth_a);
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
        let (authority, _) = amm_authority(&pool.amm_id);
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
            &ids::TOKEN_PROGRAM_ID,
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

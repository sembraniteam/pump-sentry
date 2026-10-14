//! Raydium AMM v4 post-migration swap (no OpenBook leg).
//!
//! Wire: `[0]=SPL Token, [1]=pool, [2]=authority, [3]=coin vault,
//! [4]=pc vault, [5]=source, [6]=dest, [7]=owner`;
//! data = `tag 16 ++ amount_in ++ min_out`.
//! Token-2022 vaults fail closed (see builder).

use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

/// `SwapBaseInV2` tag (upstream unpack: 16).
pub const SWAP_BASE_IN_V2_TAG: u8 = 16;
/// AMM v4 fee: 25 bps off input.
pub const AMM_V4_FEE_BPS: u64 = 25;
/// Fee denominator for [`AMM_V4_FEE_BPS`].
pub const AMM_V4_FEE_DEN: u64 = 10_000;

/// Quote `amount_in` with fee. `None` on degenerate inputs.
/// Over-limit fees refuse (fail-closed); the on-chain fee itself
/// ceils, so the quote ceils the fee leg to stay conservative.
pub fn quote_base_in(
    amount_in: u64,
    in_reserve: u64,
    out_reserve: u64,
    fee_bps: u64,
) -> Option<u64> {
    if amount_in == 0 || in_reserve == 0 || out_reserve == 0 {
        return None;
    }
    if fee_bps > AMM_V4_FEE_DEN {
        return None;
    }
    let fee = amount_in as u128 * fee_bps as u128;
    let fee = fee.div_ceil(AMM_V4_FEE_DEN as u128);
    let eff_in = (amount_in as u128).checked_sub(fee)?;
    if eff_in == 0 {
        return None;
    }
    let out =
        eff_in * out_reserve as u128 / (in_reserve as u128 + eff_in);
    u64::try_from(out).ok().filter(|o| *o > 0)
}

/// Quote with the standard fee.
pub fn quote_base_in_std(amount_in: u64, in_reserve: u64, out_reserve: u64) -> Option<u64> {
    quote_base_in(amount_in, in_reserve, out_reserve, AMM_V4_FEE_BPS)
}

/// Slippage floor. `None` on degenerate input or zero floor.
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

/// AMM vault authority for a pool nonce. Proof helper, not dispatch
/// (dispatch passes the constant). `None` if on-curve.
pub fn amm_authority_for_nonce(nonce: u8) -> Option<Pubkey> {
    Pubkey::create_program_address(&[b"amm authority", &[nonce]], &ids::RAYDIUM_V4_PROGRAM_ID).ok()
}

/// Build a `SwapBaseInV2` ix. Caller-resolved addresses only; no RPC.
/// [0] is always classic SPL Token: upstream hard-requires it, so
/// Token-2022 vaults are unroutable. `None` on zero `amount_in`.
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
        // 2% over 1M floors to 980k; dust and full-slippage refuse.
        assert_eq!(raydium_sell_floor(1_000_000, 200), Some(980_000));
        assert_eq!(raydium_sell_floor(1_000_000, 0), Some(1_000_000));
        assert_eq!(raydium_sell_floor(0, 200), None);
        assert_eq!(raydium_sell_floor(1, 9_999), None);
        assert_eq!(raydium_sell_floor(1_000_000, 10_000), None);
        assert_eq!(raydium_sell_floor(1_000_000, 10_001), None);
    }

    #[test]
    fn quote_takes_fee_and_rejects_degenerate() {
        // 1M in, 100M/100M, 25bps → 987_648 (rounded down).
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
        // Upstream hard-requires `spl_token::id()`; no program parameter exists.
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
        // Constant across pools — never derive per pool (refuted by swap `3Kg6CRL...davX`).
        assert_ne!(ids::RAYDIUM_VAULT_AUTHORITY, Pubkey::default());
        assert_eq!(
            ids::RAYDIUM_VAULT_AUTHORITY.to_string(),
            "5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1"
        );
    }

    /// Fixture bytes by name; unknown names panic.
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

    /// Nonce byte of a V4 pool (`AmmInfo.nonce` at 8..16).
    fn fixture_nonce(data: &[u8]) -> u8 {
        u64::from_le_bytes(data[8..16].try_into().expect("nonce bytes")) as u8
    }

    #[test]
    fn derives_frozen_pool_authorities() {
        // Both frozen pools' nonces derive the shared constant.
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

    /// Frozen Fartcoin pool replays into the builder, offline.
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
        // Holder legs are synthetic (offline replay); only pool slots asserted.
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

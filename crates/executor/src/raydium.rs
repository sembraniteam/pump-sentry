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
//! post-migration pools the hunt already resolved. Live wiring (pool
//! authority resolution, funding decision) is intentionally NOT here:
//! finishing that path needs a funded acceptance run first. Paper code
//! must not route through this module without that run.

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

/// Build a Raydium AMM v4 `SwapBaseInV2` instruction. All addresses are
/// caller-resolved (pool vaults from [`crate::RAYDIUM_V4_PROGRAM_ID`]-owned
/// parsing, authority derived off-chain); this function only lays out the
/// verified wire order — it performs no RPC and never panics. `None` on a
/// zero `amount_in` (a doomed ix: the chain would fail it after fees).
/// Dormant path: post-migration tracking is paper-only, and Token-2022
/// pools need a program parameter before this is routable — do not wire
/// it without a funded run proving both.
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
}

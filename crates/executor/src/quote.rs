//! Fee-inclusive bonding-curve quotes, native (no SDK dependency).
//!
//! Two differences from the reference:
//! - over-remainder buys return [`QuoteError::DepletedCurve`], not a cap
//!   (our v3 buys omit `partial_fill`).
//! - guards use ceiling division; the reference floors (dust 6002).
//!
//! Scope: pre-completion mayhem + standard curves. Cashback, complete,
//! pool legs, AMM quotes are [`QuoteError::Unsupported`].
//!
//! Layouts follow IDL field order: curve offsets, Global 1088 with buyback
//! at 741..997, FeeConfig trailing zeros ignored.

use thiserror::Error;

/// Quote failures. Every variant fails dispatch closed upstream.
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteError {
    /// Reserves empty or quote unresolvable.
    #[error("curve reserves empty")]
    EmptyReserves,
    /// Amount exceeds the curve remainder.
    #[error("amount exceeds curve remainder")]
    DepletedCurve,
    #[error("fees exceed output")]
    FeesExceedOutput,
    /// Tier table empty (program: FeeTiersEmpty).
    #[error("fee tier table empty")]
    EmptyFeeTiers,
    #[error("zero amount")]
    ZeroAmount,
    /// Overflow (never saturate a guard).
    #[error("integer overflow")]
    MathOverflow,
    /// Cashback / complete / pool-leg / AMM.
    #[error("unsupported venue")]
    Unsupported,
    #[error("bad account bytes")]
    BadAccount,
}

/// Fee triple in bps (IDL order: lp, protocol, creator).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FeesBps {
    /// LP fee (unused on curves; parsed for completeness).
    pub lp: u64,
    pub protocol: u64,
    pub creator: u64,
}

/// One fee tier: mcap threshold (lamports) + fees above it.
pub type FeeTierRow = (u128, FeesBps);

/// Fee schedule from the FeeConfig account.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FeeSchedule {
    /// SOL-like tiers.
    pub tiers: Vec<FeeTierRow>,
    /// USDC tiers.
    pub stable: Vec<FeeTierRow>,
    /// Flat fallback.
    pub flat: FeesBps,
    /// Exotic flat (all-zero = unset -> flat).
    pub exotic: FeesBps,
}

/// Curve inputs for quoting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurveView {
    /// Virtual token reserves.
    pub vt: u64,
    /// Virtual quote reserves.
    pub vq: u64,
    /// Real (unbought) tokens: the hard cap.
    pub real_tokens: u64,
    /// Coin creator rate (0 = unconfigured).
    pub coin_creator_fee_bps: u64,
    /// Creator is the default key: the program charges no creator
    /// fee regardless of schedule (per SDK `v3Rates`).
    pub creator_is_default: bool,
    /// Schedule selector.
    pub quote: QuoteKind,
}

/// Quote-mint class selecting the schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteKind {
    /// SOL-like: `fee_tiers`.
    SolLike,
    /// USDC: `stable_fee_tiers`.
    Usdc,
    /// Else exotic-or-flat.
    Other,
}

/// Max decoded tiers per table (DoS bound; live uses 1+1).
/// Single source is [`ids::MAX_FEE_TIERS`]; re-exported here.
pub use ids::MAX_FEE_TIERS;

/// Decode [`FeeSchedule`]. Layout: disc(8) + bump(1) + admin(32) + flat(24)
/// + fee vec + stable vec + exotic(24, zeros when predating the extension).
pub fn parse_fee_schedule(data: &[u8]) -> Result<FeeSchedule, QuoteError> {
    let off = 8usize + 1 + 32 + 24;
    if data.len() < off + 4 {
        return Err(QuoteError::BadAccount);
    }
    let flat = FeesBps {
        lp: u64le(data, 41)?,
        protocol: u64le(data, 49)?,
        creator: u64le(data, 57)?,
    };
    let (tiers, next) = parse_tier_vec(data, off)?;
    let (stable, next) = parse_tier_vec(data, next)?;
    // Pre-extension accounts (4073 bytes) end here: missing exotic
    // reads as unset, like the reference.
    let exotic = if data.len() >= next + 24 {
        FeesBps {
            lp: u64le(data, next)?,
            protocol: u64le(data, next + 8)?,
            creator: u64le(data, next + 16)?,
        }
    } else if data.len() == next {
        FeesBps::default()
    } else {
        return Err(QuoteError::BadAccount);
    };
    Ok(FeeSchedule { tiers, stable, flat, exotic })
}

/// Creator-fee gate from Global byte 1045. `Err` on short/bad bytes.
pub fn parse_fee_gate(data: &[u8]) -> Result<bool, QuoteError> {
    let b = *data.get(1045).ok_or(QuoteError::BadAccount)?;
    match b {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(QuoteError::BadAccount),
    }
}

fn parse_tier_vec(data: &[u8], off: usize) -> Result<(Vec<FeeTierRow>, usize), QuoteError> {
    let n = u32le(data, off)? as usize;
    if n > MAX_FEE_TIERS {
        return Err(QuoteError::BadAccount);
    }
    let mut out = Vec::with_capacity(n.min(16));
    let mut at = off + 4;
    for _ in 0..n {
        let threshold = u128le(data, at)?;
        let fees = FeesBps {
            lp: u64le(data, at + 16)?,
            protocol: u64le(data, at + 24)?,
            creator: u64le(data, at + 32)?,
        };
        out.push((threshold, fees));
        at += 40;
    }
    Ok((out, at))
}

fn u64le(data: &[u8], off: usize) -> Result<u64, QuoteError> {
    data.get(off..off + 8)
        .and_then(|b| <[u8; 8]>::try_from(b).ok())
        .map(u64::from_le_bytes)
        .ok_or(QuoteError::BadAccount)
}

fn u32le(data: &[u8], off: usize) -> Result<u32, QuoteError> {
    data.get(off..off + 4)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map(u32::from_le_bytes)
        .ok_or(QuoteError::BadAccount)
}

fn u128le(data: &[u8], off: usize) -> Result<u128, QuoteError> {
    data.get(off..off + 16)
        .and_then(|b| <[u8; 16]>::try_from(b).ok())
        .map(u128::from_le_bytes)
        .ok_or(QuoteError::BadAccount)
}

/// `ceil(a / b)` for non-zero `b`. Crate-private: only the quote
/// builders below use it.
#[inline]
fn ceil_div(a: u128, b: u128) -> Result<u128, QuoteError> {
    if b == 0 {
        return Err(QuoteError::MathOverflow);
    }
    a.checked_add(b - 1)
        .and_then(|v| v.checked_div(b))
        .ok_or(QuoteError::MathOverflow)
}

/// Basis-point denominator shared by fee and slippage math (u128
/// domain here; single source is `ids::BPS_DENOMINATOR`).
pub const BPS_DENOMINATOR: u128 = ids::BPS_DENOMINATOR as u128;

/// `ceil(amount * bps / 10_000)`. Fees round up (flooring dust-underfunds: 6002).
#[inline]
fn fee_amount(amount: u128, bps: u64) -> Result<u128, QuoteError> {
    let prod = amount
        .checked_mul(u128::from(bps))
        .ok_or(QuoteError::MathOverflow)?;
    ceil_div(prod, BPS_DENOMINATOR)
}

/// `a * b / c` floored. Overflow is `Err`.
#[inline]
fn mul_div(a: u128, b: u128, c: u128) -> Result<u128, QuoteError> {
    a.checked_mul(b)
        .ok_or(QuoteError::MathOverflow)?
        .checked_div(c)
        .ok_or(QuoteError::MathOverflow)
}

/// Constant-product exact-in swap.
#[inline]
fn swap_exact_input(amount: u128, in_reserve: u128, out_reserve: u128) -> Result<u128, QuoteError> {
    let denom = in_reserve
        .checked_add(amount)
        .ok_or(QuoteError::MathOverflow)?;
    mul_div(amount, out_reserve, denom)
}

/// Guard ceiling for every spend guard. Zero, overflow, or slippage >= 100%
/// (unguarded 2x) -> `None` (fail-closed, mirrors the sell floor policy).
pub fn slippage_ceiling(quoted_cost_incl_fees: u64, slippage_bps: u64) -> Option<u64> {
    if quoted_cost_incl_fees == 0 {
        return None;
    }
    if u128::from(slippage_bps) >= BPS_DENOMINATOR {
        return None;
    }
    let bps = u128::from(slippage_bps);
    let guard = u128::from(quoted_cost_incl_fees)
        .checked_mul(BPS_DENOMINATOR + bps)
        .and_then(|v| v.checked_add(9_999))
        .and_then(|v| v.checked_div(BPS_DENOMINATOR))?;
    u64::try_from(guard).ok().filter(|g| *g > 0)
}

/// Bonding-curve mcap in lamports.
#[inline]
pub fn market_cap(vq: u64, vt: u64, supply: u64) -> Result<u128, QuoteError> {
    if vt == 0 {
        return Err(QuoteError::EmptyReserves);
    }
    mul_div(u128::from(vq), u128::from(supply), u128::from(vt))
}

/// Highest tier at or below `mcap`, else the first. `None` when empty.
pub fn tier_for(tiers: &[(u128, FeesBps)], mcap: u128) -> Option<&FeesBps> {
    let first = tiers.first()?;
    if mcap < first.0 {
        return Some(&first.1);
    }
    let mut picked: &FeesBps = &first.1;
    for (threshold, fees) in tiers.iter() {
        if mcap >= *threshold {
            picked = fees;
        }
    }
    Some(picked)
}

/// `(protocol, creator)` bps. Schedule by quote kind + mcap, then the coin
/// rate wins while the gate is on and nonzero. Global bps fields unused.
pub fn curve_fee_bps(
    schedule: &FeeSchedule,
    gate_on: bool,
    view: &CurveView,
    mint_supply: u64,
) -> Result<(u64, u64), QuoteError> {
    let mcap = market_cap(view.vq, view.vt, mint_supply)?;
    let fees = match view.quote {
        QuoteKind::SolLike => tier_for(&schedule.tiers, mcap).ok_or(QuoteError::EmptyFeeTiers)?,
        QuoteKind::Usdc => tier_for(&schedule.stable, mcap).ok_or(QuoteError::EmptyFeeTiers)?,
        QuoteKind::Other => {
            let e = &schedule.exotic;
            if e.lp == 0 && e.protocol == 0 && e.creator == 0 {
                &schedule.flat
            } else {
                e
            }
        }
    };
    let creator =
        if gate_on && view.coin_creator_fee_bps != 0 { view.coin_creator_fee_bps } else { fees.creator };
    // Creatorless curves charge no creator fee (program rule).
    let creator = if view.creator_is_default { 0 } else { creator };
    Ok((fees.protocol, creator))
}

/// Fee-inclusive cost to buy `amount`. Over-remainder fails closed.
pub fn buy_cost_incl_fees(
    schedule: &FeeSchedule,
    gate_on: bool,
    view: &CurveView,
    mint_supply: u64,
    amount: u64,
) -> Result<u64, QuoteError> {
    if amount == 0 {
        return Ok(0);
    }
    if view.vt == 0 || view.vq == 0 {
        return Err(QuoteError::EmptyReserves);
    }
    if u128::from(amount) > u128::from(view.real_tokens) {
        return Err(QuoteError::DepletedCurve);
    }
    let denom = u128::from(view.vt)
        .checked_sub(u128::from(amount))
        .filter(|d| *d > 0)
        .ok_or(QuoteError::DepletedCurve)?;
    let cost = mul_div(u128::from(amount), u128::from(view.vq), denom)?
        .checked_add(1)
        .ok_or(QuoteError::MathOverflow)?;
    let (proto, creator) = curve_fee_bps(schedule, gate_on, view, mint_supply)?;
    let total = cost
        .checked_add(fee_amount(cost, proto)?)
        .and_then(|v| v.checked_add(fee_amount(cost, creator).ok()?))
        .ok_or(QuoteError::MathOverflow)?;
    u64::try_from(total).map_err(|_| QuoteError::MathOverflow)
}

/// Proceeds for selling `amount`: out minus ceil'd fees.
pub fn sell_out_after_fees(
    schedule: &FeeSchedule,
    gate_on: bool,
    view: &CurveView,
    mint_supply: u64,
    amount: u64,
) -> Result<u64, QuoteError> {
    if amount == 0 {
        return Ok(0);
    }
    if view.vt == 0 || view.vq == 0 {
        return Err(QuoteError::EmptyReserves);
    }
    let gross = mul_div(
        u128::from(amount),
        u128::from(view.vq),
        u128::from(view.vt).checked_add(u128::from(amount)).ok_or(QuoteError::MathOverflow)?,
    )?;
    let (proto, creator) = curve_fee_bps(schedule, gate_on, view, mint_supply)?;
    let net = gross
        .checked_sub(fee_amount(gross, proto)?)
        .and_then(|v| v.checked_sub(fee_amount(gross, creator).ok()?))
        .ok_or(QuoteError::FeesExceedOutput)?;
    if net == 0 {
        return Err(QuoteError::FeesExceedOutput);
    }
    u64::try_from(net).map_err(|_| QuoteError::MathOverflow)
}

/// Tokens out for an exact quote input. `Ok(0)` when input can't cover fees.
pub fn buy_tokens_for_exact_in(    schedule: &FeeSchedule,
    gate_on: bool,
    view: &CurveView,
    mint_supply: u64,
    sol_amount: u64,
) -> Result<u64, QuoteError> {
    if sol_amount == 0 || view.vt == 0 {
        return Ok(0);
    }
    if view.vq == 0 {
        return Err(QuoteError::EmptyReserves);
    }
    let (proto, creator) = curve_fee_bps(schedule, gate_on, view, mint_supply)?;
    let total_bps = u128::from(proto) + u128::from(creator);
    let mut net = u128::from(sol_amount) * BPS_DENOMINATOR / (BPS_DENOMINATOR + total_bps);
    if net == 0 {
        return Ok(0);
    }
    let fees = fee_amount(net, proto)? + fee_amount(net, creator)?;
    if net + fees > u128::from(sol_amount) {
        net = net
            .checked_sub(net + fees - u128::from(sol_amount))
            .filter(|n| *n > 0)
            .ok_or(QuoteError::ZeroAmount)?;
    }
    let tokens = swap_exact_input(
        net.checked_sub(1).ok_or(QuoteError::ZeroAmount)?,
        u128::from(view.vq),
        u128::from(view.vt),
    )?;
    Ok(tokens.min(u128::from(view.real_tokens)) as u64)
}

/// PumpSwap pool inputs for quoting a `sell_v2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolView {
    /// Base reserve: token vault amount.
    pub base_reserve: u64,
    /// Effective quote reserve: WSOL vault plus signed virtual reserves.
    pub eff_quote_reserve: u64,
    /// Real WSOL vault minus kept-fee buckets (payout cap).
    pub real_quote_cap: u64,
    /// LP fee bps (AMM global config).
    pub lp_bps: u64,
    /// Protocol fee bps (AMM global config).
    pub proto_bps: u64,
    /// Creator fee bps (tier or pool override, zeroed when creatorless).
    pub creator_bps: u64,
}

/// Proceeds for a PumpSwap `sell_v2`: constant-product exact-in on
/// effective reserves minus ceil'd fees, capped by real reserves.
/// Mirrors `@pump-fun/pump-swap-sdk` `sellBaseInput`/`sellAmounts`.
pub fn pumpswap_sell_out(view: &PoolView, amount: u64) -> Result<u64, QuoteError> {
    if amount == 0 {
        return Ok(0);
    }
    if view.base_reserve == 0 || view.eff_quote_reserve == 0 {
        return Err(QuoteError::EmptyReserves);
    }
    let gross = swap_exact_input(
        u128::from(amount),
        u128::from(view.base_reserve),
        u128::from(view.eff_quote_reserve),
    )?;
    let without_lp = gross
        .checked_sub(fee_amount(gross, view.lp_bps)?)
        .ok_or(QuoteError::FeesExceedOutput)?;
    // Payouts come from real vault lamports, not virtual reserves.
    if u128::from(view.real_quote_cap) < without_lp {
        return Err(QuoteError::FeesExceedOutput);
    }
    let net = without_lp
        .checked_sub(fee_amount(gross, view.proto_bps)?)
        .and_then(|v| v.checked_sub(fee_amount(gross, view.creator_bps).ok()?))
        .ok_or(QuoteError::FeesExceedOutput)?;
    if net == 0 {
        return Err(QuoteError::FeesExceedOutput);
    }
    u64::try_from(net).map_err(|_| QuoteError::MathOverflow)
}

/// Slippage floor over a PumpSwap net quote (rounds down).
/// `None` on degenerate input or zero floor.
pub fn pumpswap_sell_floor(net_quote: u64, slippage_bps: u64) -> Option<u64> {
    if net_quote == 0 || u128::from(slippage_bps) >= BPS_DENOMINATOR {
        return None;
    }
    let floor = u128::from(net_quote)
        .checked_mul(BPS_DENOMINATOR - u128::from(slippage_bps))?
        .checked_div(BPS_DENOMINATOR)?;
    u64::try_from(floor).ok().filter(|f| *f > 0)
}

/// `(lp, protocol, creator)` bps for a PumpSwap sell: SOL-like tiers
/// by mcap, pool creator-rate override under the global gate, and the
/// creator leg zeroed when the pool is creatorless. Mirrors the SDK
/// `computeFeesBps` plus the default-key rule.
pub fn pumpswap_fee_bps(
    schedule: &FeeSchedule,
    gate_on: bool,
    pool_creator_fee_bps: u64,
    coin_creator_default: bool,
    mcap: u128,
) -> Result<FeesBps, QuoteError> {
    let tier = tier_for(&schedule.tiers, mcap).ok_or(QuoteError::EmptyFeeTiers)?;
    let creator = if gate_on && pool_creator_fee_bps != 0 {
        pool_creator_fee_bps
    } else {
        tier.creator
    };
    Ok(FeesBps {
        lp: tier.lp,
        protocol: tier.protocol,
        creator: if coin_creator_default { 0 } else { creator },
    })
}

/// Creator-fee gate from AMM GlobalConfig bytes. With a readable
/// fee schedule the tier rates rule (SDK `computeFeesBps`); the
/// global lp/proto fields are then unused, so only the gate parses.
/// `Err` on short/bad bytes.
pub fn parse_amm_creator_gate(data: &[u8]) -> Result<bool, QuoteError> {
    match data.get(ids::PUMP_AMM_GLOBAL_CREATOR_GATE_OFFSET) {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(QuoteError::BadAccount),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V_SOL: u64 = 30_000_000_000;
    const V_TOKENS: u64 = 1_073_000_000_000_000;
    const REAL: u64 = 793_100_000_000_000;
    /// Tier-selection supply (live-mint stand-in).
    const SUPPLY: u64 = 1_000_000_000_000_000;

    fn sched(proto: u64, creator: u64) -> FeeSchedule {
        let one = vec![(0u128, FeesBps { lp: 0, protocol: proto, creator })];
        FeeSchedule {
            tiers: one.clone(),
            stable: one.clone(),
            flat: FeesBps { lp: 0, protocol: proto, creator },
            exotic: FeesBps::default(),
        }
    }

    fn sol_curve() -> CurveView {
        CurveView {
            vt: V_TOKENS,
            vq: V_SOL,
            real_tokens: REAL,
            coin_creator_fee_bps: 0,
            creator_is_default: false,
            quote: QuoteKind::SolLike,
        }
    }

    // Over-remainder refuses (DepletedCurve), not partial-fill (see module docs).
    #[test]
    fn over_remainder_fails_closed_not_capped() {
        let s = sched(100, 50);
        assert_eq!(
            buy_cost_incl_fees(&s, false, &sol_curve(), 0, 800_000_000_000_000),
            Err(QuoteError::DepletedCurve)
        );
        // Same math on the capped remainder reproduces the SDK sum.
        assert_eq!(
            buy_cost_incl_fees(&s, false, &sol_curve(), 0, REAL),
            Ok(85_005_359_057 + 850_053_591 + 425_026_796)
        );
    }

    #[test]
    fn buy_cost_matches_reference_vectors() {
        // cost = 1e12 * 30e9 / (1073e15 - 1e12) + 1 = 27_985_075;
        // total = cost + ceil(1%) + ceil(0.5%) = 28_404_852.
        let s = sched(100, 50);
        let v = sol_curve();
        assert_eq!(
            buy_cost_incl_fees(&s, false, &v, 0, 1_000_000_000_000),
            Ok(28_404_852)
        );
        assert_eq!(buy_cost_incl_fees(&s, false, &v, 0, 0), Ok(0));
        assert_eq!(
            buy_cost_incl_fees(&s, false, &CurveView { vt: 0, ..v }, 0, 1),
            Err(QuoteError::EmptyReserves)
        );
    }

    #[test]
    fn exact_in_matches_reference_vector() {
        // 2002 in at 5bps buys 71_497_561 (fee ceil'd on floor'd net, net-1 swapped).
        let s = sched(5, 0);
        assert_eq!(
            buy_tokens_for_exact_in(&s, false, &sol_curve(), 0, 2002),
            Ok(71_497_561)
        );
        assert_eq!(buy_tokens_for_exact_in(&s, false, &sol_curve(), 0, 1), Ok(0));
        assert_eq!(buy_tokens_for_exact_in(&s, false, &sol_curve(), 0, 0), Ok(0));
    }

    #[test]
    fn sell_quote_is_constant_product_minus_ceil_fees() {
        let s = sched(100, 50);
        let v = sol_curve();
        // gross = 27_932_960; net = gross - ceil(1%) - ceil(0.5%) = 27_513_965.
        assert_eq!(
            sell_out_after_fees(&s, false, &v, 0, 1_000_000_000_000),
            Ok(27_513_965)
        );
        assert_eq!(sell_out_after_fees(&s, false, &v, 0, 0), Ok(0));
    }

    fn pool_view() -> PoolView {
        PoolView {
            base_reserve: 1_000_000_000_000,
            eff_quote_reserve: 30_000_000_000,
            real_quote_cap: 30_000_000_000,
            lp_bps: 20,
            proto_bps: 5,
            creator_bps: 30,
        }
    }

    #[test]
    fn pumpswap_sell_matches_reference_math() {
        // gross = 10e9*30e9/1010e9 = 297_029_702 (floor);
        // lp = ceil(20bps) = 594_060; proto = ceil(5bps) = 148_515;
        // creator = ceil(30bps) = 891_090;
        // net = 297_029_702 - 594_060 - 148_515 - 891_090 = 295_396_037.
        let v = pool_view();
        assert_eq!(pumpswap_sell_out(&v, 10_000_000_000), Ok(295_396_037));
        assert_eq!(pumpswap_sell_out(&v, 0), Ok(0));
        // Real-reserve cap below the LP-stripped gross refuses.
        let capped = PoolView { real_quote_cap: 100_000_000, ..v };
        assert_eq!(
            pumpswap_sell_out(&capped, 10_000_000_000),
            Err(QuoteError::FeesExceedOutput)
        );
        // Empty reserves refuse.
        let empty = PoolView { base_reserve: 0, ..v };
        assert_eq!(
            pumpswap_sell_out(&empty, 10_000_000_000),
            Err(QuoteError::EmptyReserves)
        );
    }

    #[test]
    fn pumpswap_floor_rounds_down() {
        // 295_396_037 - 200bps -> 289_488_116 (floor).
        assert_eq!(pumpswap_sell_floor(295_396_037, 200), Some(289_488_116));
        assert_eq!(pumpswap_sell_floor(295_396_037, 0), Some(295_396_037));
        assert_eq!(pumpswap_sell_floor(0, 200), None);
        assert_eq!(pumpswap_sell_floor(1, 9_999), None);
        assert_eq!(pumpswap_sell_floor(1_000_000, 10_000), None);
    }

    #[test]
    fn pumpswap_fee_bps_follows_tiers_and_gate() {
        let sched = FeeSchedule {
            tiers: vec![
                (0u128, FeesBps { lp: 20, protocol: 5, creator: 95 }),
                (1_000_000_000_000u128, FeesBps { lp: 20, protocol: 5, creator: 30 }),
            ],
            stable: vec![],
            flat: FeesBps::default(),
            exotic: FeesBps::default(),
        };
        // Below top tier: first-row creator.
        assert_eq!(
            pumpswap_fee_bps(&sched, true, 0, false, 500),
            Ok(FeesBps { lp: 20, protocol: 5, creator: 95 })
        );
        // At/above: top-row creator.
        assert_eq!(
            pumpswap_fee_bps(&sched, true, 0, false, 1_000_000_000_000),
            Ok(FeesBps { lp: 20, protocol: 5, creator: 30 })
        );
        // Pool override wins only under gate + nonzero.
        assert_eq!(
            pumpswap_fee_bps(&sched, true, 100, false, 1_000_000_000_000),
            Ok(FeesBps { lp: 20, protocol: 5, creator: 100 })
        );
        assert_eq!(
            pumpswap_fee_bps(&sched, false, 100, false, 1_000_000_000_000),
            Ok(FeesBps { lp: 20, protocol: 5, creator: 30 })
        );
        // Creatorless pool: no creator leg.
        assert_eq!(
            pumpswap_fee_bps(&sched, true, 100, true, 1_000_000_000_000),
            Ok(FeesBps { lp: 20, protocol: 5, creator: 0 })
        );
        // Empty tiers fail closed.
        let empty = FeeSchedule { tiers: vec![], ..sched };
        assert_eq!(
            pumpswap_fee_bps(&empty, true, 0, false, 0),
            Err(QuoteError::EmptyFeeTiers)
        );
    }

    #[test]
    fn amm_global_decodes_live_gate() {
        let bytes: &[u8] =
            include_bytes!("../../listener/tests/fixtures/amm_global_config_live.bin");
        assert_eq!(bytes.len(), ids::PUMP_AMM_GLOBAL_LEN);
        assert_eq!(parse_amm_creator_gate(bytes), Ok(true));
        assert_eq!(parse_amm_creator_gate(&bytes[..100]), Err(QuoteError::BadAccount));
        let mut bad_gate = bytes.to_vec();
        bad_gate[ids::PUMP_AMM_GLOBAL_CREATOR_GATE_OFFSET] = 7;
        assert_eq!(parse_amm_creator_gate(&bad_gate), Err(QuoteError::BadAccount));
    }

    #[test]
    fn amm_fee_schedule_decodes_live_tiers() {
        // Same FeeConfig layout as the curve schedule: 25 live tiers.
        let bytes: &[u8] =
            include_bytes!("../../listener/tests/fixtures/amm_fee_config_live.bin");
        let sched = parse_fee_schedule(bytes).expect("live AMM schedule decodes");
        assert_eq!(sched.tiers.len(), 25);
        assert_eq!(
            sched.tiers[0],
            (0u128, FeesBps { lp: 2, protocol: 93, creator: 30 })
        );
        assert_eq!(
            sched.tiers[1],
            (420_000_000_000u128, FeesBps { lp: 20, protocol: 5, creator: 95 })
        );
    }

    #[test]
    fn schedule_selection_follows_quote_kind() {
        let mk = |t: Vec<FeeTierRow>| FeeSchedule {
            tiers: t.clone(),
            stable: vec![(0u128, FeesBps { lp: 0, protocol: 70, creator: 10 })],
            flat: FeesBps { lp: 0, protocol: 5, creator: 0 },
            exotic: FeesBps::default(),
        };
        // Tier pick: highest threshold <= mcap else first.
        let s = mk(vec![
            (0u128, FeesBps { lp: 0, protocol: 90, creator: 30 }),
            (1_000u128, FeesBps { lp: 0, protocol: 80, creator: 20 }),
        ]);
        let v = sol_curve();
        assert_eq!(curve_fee_bps(&s, false, &v, SUPPLY), Ok((80, 20)));
        // USDC routes to stable; other quotes to flat when exotic unset.
        let u = CurveView { quote: QuoteKind::Usdc, ..v };
        assert_eq!(curve_fee_bps(&s, false, &u, SUPPLY), Ok((70, 10)));
        let o = CurveView { quote: QuoteKind::Other, ..v };
        assert_eq!(curve_fee_bps(&s, false, &o, SUPPLY), Ok((5, 0)));
        // Empty SOL tiers fail closed (program: FeeTiersEmpty).
        let e = mk(vec![]);
        assert_eq!(
            curve_fee_bps(&e, false, &v, SUPPLY),
            Err(QuoteError::EmptyFeeTiers)
        );
        // Coin override wins only with gate on + nonzero stored rate.
        let w = CurveView { coin_creator_fee_bps: 250, ..v };
        assert_eq!(curve_fee_bps(&s, true, &w, SUPPLY), Ok((80, 250)));
        assert_eq!(curve_fee_bps(&s, false, &w, SUPPLY), Ok((80, 20)));
        let z = CurveView { coin_creator_fee_bps: 0, ..v };
        assert_eq!(curve_fee_bps(&s, true, &z, SUPPLY), Ok((80, 20)));
        // Creatorless curves charge no creator leg at all.
        let d = CurveView { creator_is_default: true, ..w };
        assert_eq!(curve_fee_bps(&s, true, &d, SUPPLY), Ok((80, 0)));
    }

    #[test]
    fn fee_schedule_decodes_live_prefix() {
        // Live FeeConfig bytes (4097 bytes, trailing zero room): single (0, 95, 30).
        let bytes: &[u8] = include_bytes!("../../listener/tests/fixtures/fee_config_live.bin");
        assert_eq!(bytes.len(), 4097);
        let sched = parse_fee_schedule(bytes).expect("live schedule decodes");
        let one = vec![(0u128, FeesBps { lp: 0, protocol: 95, creator: 30 })];
        assert_eq!(sched.tiers, one);
        assert_eq!(sched.stable, one);
        assert_eq!(sched.flat, FeesBps { lp: 0, protocol: 95, creator: 30 });
        assert_eq!(sched.exotic, FeesBps { lp: 0, protocol: 95, creator: 30 });
        // Hostile / short buffers fail closed.
        assert_eq!(parse_fee_schedule(&bytes[..72]), Err(QuoteError::BadAccount));
        let mut hostile = bytes[..177].to_vec();
        hostile[65..69].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse_fee_schedule(&hostile), Err(QuoteError::BadAccount));
    }

    #[test]
    fn fee_gate_parses_boolean_byte() {
        let mut data = vec![0u8; 1088];
        data[1045] = 1;
        assert_eq!(parse_fee_gate(&data), Ok(true));
        data[1045] = 0;
        assert_eq!(parse_fee_gate(&data), Ok(false));
        data[1045] = 7;
        assert_eq!(parse_fee_gate(&data), Err(QuoteError::BadAccount));
        assert_eq!(parse_fee_gate(&data[..1045]), Err(QuoteError::BadAccount));
    }

    #[test]
    fn guard_ceiling_never_underfunds() {
        // 1_668_970 + 100bps → 1_685_660.
        assert_eq!(slippage_ceiling(1_668_970, 100), Some(1_685_660));
        assert_eq!(slippage_ceiling(10_000, 1), Some(10_001));
        assert_eq!(slippage_ceiling(0, 200), None);
        assert_eq!(slippage_ceiling(1_000, 9_999), Some(2_000));
        assert_eq!(slippage_ceiling(1_000, 10_000), None);
        assert_eq!(slippage_ceiling(1_000, 99_999), None);
        assert_eq!(slippage_ceiling(u64::MAX, 10_000), None);
        assert_eq!(slippage_ceiling(u64::MAX, 0), Some(u64::MAX));
    }
}

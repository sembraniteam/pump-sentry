//! Fee-inclusive bonding-curve quotes, implemented natively (no SDK
//! dependency — `pump-rust-client` 0.4.0 `math/bonding_curve.rs` +
//! `math/fees.rs` + `math/utils.rs` were used as REFERENCE ONLY).
//!
//! Every formula below mirrors the reference SDK exactly; the two
//! deliberate differences are documented where they occur:
//! - over-remainder buys return [`QuoteError::DepletedCurve`] instead
//!   of the SDK's partial-fill capping. Our v3 buys omit `partial_fill`
//!   (a fill past the remainder would complete the curve and strand
//!   the position pre-Raydium-exit), so a capped quote would bless a
//!   guard for tokens the program will refuse (`NotEnoughTokensToBuy`).
//!   Fail closed here; the dispatch layer treats it like any other
//!   unroutable order.
//! - guards use ceiling division (see `slippage_ceiling`), where the
//!   reference uses floor: a floored guard underfunds by a lamport and
//!   fails with 6002 (the frozen failed sample missed by 1.15%).
//!
//! Scope: pre-completion mayhem + standard curves. Post-completion
//! pool legs, multi-hop hops, and AMM quotes are OUT →
//! [`QuoteError::Unsupported`]. Cashback/complete curves are refused
//! (`Unsupported`) — the router must not call here for them.
//!
//! Account layouts come from the official IDL field order (rustdoc
//! `BondingCurve`/`Global`/`FeeConfig`/`Fees`/`FeeTier`), cross-checked
//! against live mainnet bytes: curve creator/flags/quote/complete
//! offsets, Global length 1088 with the buyback array at 741..997
//! matching the published 8-list in order, live FeeConfig decoding to
//! a single (0, 95, 30) tier schedule. Trailing zeros in FeeConfig
//! are ignored (forward-compatible room for more tiers, exactly like
//! the reference `AccountWrapper`).

use thiserror::Error;

/// Quote failures: every variant fails the dispatch closed upstream.
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteError {
    /// Curve reserves empty (migrated) or quote mint unresolvable.
    #[error("curve reserves empty")]
    EmptyReserves,
    /// Amount exceeds the curve remainder (no pool-leg pricing here).
    #[error("amount exceeds curve remainder")]
    DepletedCurve,
    /// Fees exceed the sell output.
    #[error("fees exceed output")]
    FeesExceedOutput,
    /// Fee tier table empty for this quote (program: FeeTiersEmpty).
    #[error("fee tier table empty")]
    EmptyFeeTiers,
    /// Zero amount in.
    #[error("zero amount")]
    ZeroAmount,
    /// Integer overflow (fail closed, never saturate a guard).
    #[error("integer overflow")]
    MathOverflow,
    /// Unsupported venue (cashback/complete/pool-leg/AMM).
    #[error("unsupported venue")]
    Unsupported,
    /// Bad account bytes (wrong length, hostile vec length, bad flag).
    #[error("bad account bytes")]
    BadAccount,
}

/// Fee triple in basis points (IDL `Fees` order: lp, protocol, creator).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FeesBps {
    /// LP fee (unused on bonding curves; parsed for completeness).
    pub lp: u64,
    /// Protocol fee.
    pub protocol: u64,
    /// Creator fee.
    pub creator: u64,
}

/// One fee tier: market-cap threshold (lamports) + fees above it.
pub type FeeTierRow = (u128, FeesBps);

/// Fee schedule decoded from the FeeConfig account.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FeeSchedule {
    /// SOL-like quote tiers (threshold → fees).
    pub tiers: Vec<FeeTierRow>,
    /// USDC quote tiers.
    pub stable: Vec<FeeTierRow>,
    /// Flat fallback (non-pump-exotic quotes, unset exotic schedule).
    pub flat: FeesBps,
    /// Exotic-quote flat schedule (all-zero = unset → flat fallback).
    pub exotic: FeesBps,
}

/// Curve inputs for quoting, sourced from curve state (+ mint supply
/// for non-mayhem curves).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurveView {
    /// Virtual token reserves.
    pub vt: u64,
    /// Virtual quote reserves.
    pub vq: u64,
    /// Real (unbought) token reserves — the hard cap.
    pub real_tokens: u64,
    /// Stored total supply (fee-tier market-cap input on mayhem).
    pub token_total_supply: u64,
    /// Coin's own creator rate (0 = unconfigured → schedule rate).
    pub coin_creator_fee_bps: u64,
    /// Quote kind selecting the schedule.
    pub quote: QuoteKind,
    /// Mayhem-mode coin (tier supply + partial-fill semantics).
    pub is_mayhem: bool,
}

/// Quote-mint class selecting the fee schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteKind {
    /// SOL-like (zero key, WSOL): `fee_tiers`.
    SolLike,
    /// USDC: `stable_fee_tiers`.
    Usdc,
    /// Anything else: exotic-or-flat.
    Other,
}

/// Upper bound on decoded fee tiers per table (DoS bound on hostile
/// vec lengths; live uses 1+1).
pub const MAX_FEE_TIERS: usize = 1024;

/// Decode a [`FeeSchedule`] from FeeConfig account bytes (trailing
/// zero room for future tiers is ignored). Pure; `Err(BadAccount)` on
/// short buffers or hostile vec lengths. Layout: disc(8) + bump(1) +
/// admin(32) + flat(24) + fee_tiers vec + stable vec + exotic(24,
/// zeros when the account predates the extension).
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
    // reads as unset (zeros → flat fallback), like the reference.
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

/// Parse the creator-fee gate from Global account bytes: byte 1045
/// (`creator_fee_configurable`; live-verified offset — the 1088-byte
/// layout is proven transitively by the buyback array at 741..997).
/// Pure; `Err(BadAccount)` on short buffers or non-boolean bytes.
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

/// `ceil(a / b)` for non-zero `b`.
#[inline]
pub fn ceil_div(a: u128, b: u128) -> Result<u128, QuoteError> {
    if b == 0 {
        return Err(QuoteError::MathOverflow);
    }
    a.checked_add(b - 1)
        .and_then(|v| v.checked_div(b))
        .ok_or(QuoteError::MathOverflow)
}

/// `ceil(amount * bps / 10_000)` — fees round UP (the program charges
/// ceil; flooring underfunds the guard by dust and fails with 6002).
#[inline]
pub fn fee_amount(amount: u128, bps: u64) -> Result<u128, QuoteError> {
    let prod = amount
        .checked_mul(u128::from(bps))
        .ok_or(QuoteError::MathOverflow)?;
    ceil_div(prod, 10_000)
}

/// `a * b / c` floored, overflow → `Err` (never saturate money math).
#[inline]
pub fn mul_div(a: u128, b: u128, c: u128) -> Result<u128, QuoteError> {
    a.checked_mul(b)
        .ok_or(QuoteError::MathOverflow)?
        .checked_div(c)
        .ok_or(QuoteError::MathOverflow)
}

/// Constant-product exact-in swap, `floor(amount * out / (in + amount))`.
#[inline]
pub fn swap_exact_input(amount: u128, in_reserve: u128, out_reserve: u128) -> Result<u128, QuoteError> {
    let denom = in_reserve
        .checked_add(amount)
        .ok_or(QuoteError::MathOverflow)?;
    mul_div(amount, out_reserve, denom)
}

/// Guard ceiling over a fee-inclusive quote: `ceil(cost * (10_000 +
/// bps) / 10_000)`. Single constructor for every spend guard —
/// [`crate::live`] equivalent lives here so executor and binary can
/// never drift apart (`bps` clamped to 10_000 — deliberate, not silent:
/// library callers passing >100% get 100%, so validate configs upstream;
/// the binary refuses `>=10000` in live mode. zero/overflow → `None`
/// = fail closed, never an unguarded dispatch).
pub fn slippage_ceiling(quoted_cost_incl_fees: u64, slippage_bps: u64) -> Option<u64> {
    if quoted_cost_incl_fees == 0 {
        return None;
    }
    let bps = u128::from(slippage_bps.min(10_000));
    let guard = u128::from(quoted_cost_incl_fees)
        .checked_mul(10_000 + bps)
        .and_then(|v| v.checked_add(9_999))
        .and_then(|v| v.checked_div(10_000))?;
    u64::try_from(guard).ok().filter(|g| *g > 0)
}

/// Bonding-curve market cap in lamports:
/// `vQuote * mintSupply / vTokens`.
#[inline]
pub fn market_cap(vq: u64, vt: u64, supply: u64) -> Result<u128, QuoteError> {
    if vt == 0 {
        return Err(QuoteError::EmptyReserves);
    }
    mul_div(u128::from(vq), u128::from(supply), u128::from(vt))
}

/// Highest tier with threshold `<= mcap`, else the first tier; `None`
/// on an empty table (program: `FeeTiersEmpty`).
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

/// `(protocol, creator)` bps for a trade: schedule by quote kind and
/// market cap, then the coin's own creator rate wins while the global
/// gate is on and the stored rate is nonzero (0 = unconfigured).
/// Global's own bps fields play no part in on-chain pricing.
pub fn curve_fee_bps(
    schedule: &FeeSchedule,
    gate_on: bool,
    view: &CurveView,
    mint_supply: u64,
) -> Result<(u64, u64), QuoteError> {
    let mcap = market_cap(view.vq, view.vt, view.tier_supply(mint_supply))?;
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
    Ok((fees.protocol, creator))
}

impl CurveView {
    /// Supply the fee-tier market cap uses: stored total on mayhem,
    /// live mint supply otherwise (program `BondingCurve::market_cap`).
    fn tier_supply(&self, mint_supply: u64) -> u64 {
        if self.is_mayhem { self.token_total_supply } else { mint_supply }
    }
}

/// Fee-inclusive quote cost (lamports) to buy `amount` base units:
/// `cost = amount * vq / (vt - amount) + 1`, plus ceil'd protocol and
/// creator fees. Over-remainder amounts fail closed ([`QuoteError::DepletedCurve`])
/// instead of the reference SDK's partial-fill capping — our v3 buys
/// omit `partial_fill`, so the program would refuse anyway.
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

/// Quote proceeds (lamports) for selling `amount`: constant-product
/// out minus ceil'd fees. `Ok(0)` on zero input.
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

/// Tokens out for an exact quote input, after the program's exact-in
/// fee derivation (floor net, per-component ceil fees, shave on
/// overshoot) and `swap(net - 1)`, capped at the curve remainder.
/// `Ok(0)` when the input can't cover fees. Used to validate
/// exact-quote `min_tokens_out` bounds.
pub fn buy_tokens_for_exact_in(
    schedule: &FeeSchedule,
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
    let mut net = u128::from(sol_amount) * 10_000 / (10_000 + total_bps);
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

#[cfg(test)]
mod tests {
    use super::*;

    const V_SOL: u64 = 30_000_000_000;
    const V_TOKENS: u64 = 1_073_000_000_000_000;
    const REAL: u64 = 793_100_000_000_000;
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
            token_total_supply: SUPPLY,
            coin_creator_fee_bps: 0,
            quote: QuoteKind::SolLike,
            is_mayhem: true,
        }
    }

    // Reference-SDK vectors (fresh SOL curve, single 100/50 tier):
    // 800T past the 793.1T remainder costs buy_quote 85_005_359_057
    // plus ceil'd 1% + 0.5% fees — our engine computes the same math
    // on the capped amount but REFUSES over-remainder (DepletedCurve)
    // instead of partial-filling (deliberate, documented above).
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
        // Independent python cross-check of the same formulas:
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
        // SDK vector: 2002 in at 5bps buys 71_497_561 (fee ceil'd on
        // the floor'd net, net-1 swapped).
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
        // Independent python cross-check: gross = 27_932_960;
        // net = gross - ceil(1%) - ceil(0.5%) = 27_513_965.
        assert_eq!(
            sell_out_after_fees(&s, false, &v, 0, 1_000_000_000_000),
            Ok(27_513_965)
        );
        assert_eq!(sell_out_after_fees(&s, false, &v, 0, 0), Ok(0));
    }

    #[test]
    fn schedule_selection_follows_quote_kind() {
        let mk = |t: Vec<FeeTierRow>| FeeSchedule {
            tiers: t.clone(),
            stable: vec![(0u128, FeesBps { lp: 0, protocol: 70, creator: 10 })],
            flat: FeesBps { lp: 0, protocol: 5, creator: 0 },
            exotic: FeesBps::default(),
        };
        // Tier pick: highest threshold <= mcap else first; mcap here is
        // ~27.96M lamports (30e9 * 1e15 / 1073e15).
        let s = mk(vec![
            (0u128, FeesBps { lp: 0, protocol: 90, creator: 30 }),
            (1_000u128, FeesBps { lp: 0, protocol: 80, creator: 20 }),
        ]);
        let v = sol_curve();
        assert_eq!(curve_fee_bps(&s, false, &v, 0), Ok((80, 20)));
        // USDC routes to stable; other quotes to flat when exotic unset.
        let u = CurveView { quote: QuoteKind::Usdc, ..v };
        assert_eq!(curve_fee_bps(&s, false, &u, 0), Ok((70, 10)));
        let o = CurveView { quote: QuoteKind::Other, ..v };
        assert_eq!(curve_fee_bps(&s, false, &o, 0), Ok((5, 0)));
        // Empty SOL tiers fail closed (program: FeeTiersEmpty).
        let e = mk(vec![]);
        assert_eq!(
            curve_fee_bps(&e, false, &v, 0),
            Err(QuoteError::EmptyFeeTiers)
        );
        // Coin override wins only with gate on + nonzero stored rate.
        let w = CurveView { coin_creator_fee_bps: 250, ..v };
        assert_eq!(curve_fee_bps(&s, true, &w, 0), Ok((80, 250)));
        assert_eq!(curve_fee_bps(&s, false, &w, 0), Ok((80, 20)));
        let z = CurveView { coin_creator_fee_bps: 0, ..v };
        assert_eq!(curve_fee_bps(&s, true, &z, 0), Ok((80, 20)));
    }

    #[test]
    fn fee_schedule_decodes_live_prefix() {
        // Live FeeConfig bytes (fetched 2026-10-14, owner = fee program,
        // 4097 bytes with trailing zero room): single (0, 95, 30)
        // schedule everywhere. The trailing zeros after the exotic
        // schedule must not disturb decoding.
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
        // Frozen 6002 vector: 1_668_970 + 100bps → 1_685_660.
        assert_eq!(slippage_ceiling(1_668_970, 100), Some(1_685_660));
        assert_eq!(slippage_ceiling(10_000, 1), Some(10_001));
        assert_eq!(slippage_ceiling(0, 200), None);
        assert_eq!(slippage_ceiling(1_000, 99_999), Some(2_000));
        assert_eq!(slippage_ceiling(u64::MAX, 10_000), None);
        assert_eq!(slippage_ceiling(u64::MAX, 0), Some(u64::MAX));
    }
}

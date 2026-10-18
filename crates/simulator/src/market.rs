//! Slippage fills and exit triggers.

use crate::types::{ExitReason, Position, Side};

/// Standard venue fee per side, bps. Live-launch schedule total
/// (95 + 30); custom-tier mints override per mint.
pub const STANDARD_VENUE_FEE_BPS: f64 = 125.0;

/// Basis-point denominator. Mirror of `ids::BPS_DENOMINATOR` (this
/// crate stays dependency-free); the parity test in the root crate
/// pins the two quote paths against each other.
const BPS_DENOMINATOR: u128 = 10_000;
/// Lamports per SOL. Mirror of `curve::layout::LAMPORTS_PER_SOL`.
const LAMPORTS_PER_SOL: f64 = 1_000_000_000.0;

/// Exact bonding-curve execution state for one mint (lamports / base
/// units).
///
/// Mirrors the curve-account fields the dispatch quote math reads
/// (`executor::quote`), so paper fills and live dispatch floors price
/// the same curve. Quote math is integer (u128, like the chain); the
/// only float legs are the USD conversions at the edges. A mint whose
/// tick carried no reserves quotes on the proxy basis instead.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CurveReserves {
    /// Virtual SOL reserves, lamports.
    pub vq_lamports: u64,
    /// Virtual token reserves, base units.
    pub vt_base: u64,
    /// Real (unbought) tokens, base units: the hard buy cap.
    pub real_tokens: u64,
    /// Mint decimals (base units per whole token).
    pub decimals: u8,
    /// SOL/USD at the quote instant (lamports <-> book USD).
    pub sol_usd: f64,
}

/// Mark-to-market quote for one mint.
#[derive(Debug, Clone, Copy)]
pub struct Mark {
    /// Spot USD per whole token.
    pub price: f64,
    /// SOL-side depth in USD (proxy basis when `curve` is `None`).
    pub liquidity_usd: f64,
    /// Exact curve state for size-aware quotes, when the tick carried
    /// reserves. `None` = proxy basis only (mocks, synthetic prices).
    pub curve: Option<CurveReserves>,
}

impl Mark {
    /// Spot price with SOL-side depth (proxy basis, no curve state).
    pub fn new(price: f64, liquidity_usd: f64) -> Self {
        Self {
            price,
            liquidity_usd,
            curve: None,
        }
    }

    /// Price without liquidity or curve state.
    pub fn price_only(price: f64) -> Self {
        Self::new(price, f64::NAN)
    }

    /// Attach exact curve reserves: fills become state- and size-aware.
    pub fn with_curve(mut self, curve: CurveReserves) -> Self {
        self.curve = Some(curve);
        self
    }
}

/// Fixed-size entry result: the whole tokens a ticket buys and their
/// fee-inclusive price (fees are inside the price, never a second
/// charge).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EntryFill {
    /// Whole tokens bought (sub-token dust floors away).
    pub quantity: u64,
    /// Fee-inclusive USD per whole token.
    pub price: f64,
}

/// Why an entry cannot be priced as a fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryFillError {
    /// Inputs unquotable (bad ticket, no SOL/USD, absurd decimals).
    Unpriceable,
    /// The size floors to zero whole tokens.
    Dust,
    /// The curve cannot absorb the ticket (drained or depleted).
    Exhausted,
}

/// `ceil(amount * bps / 10_000)`. Fees round up, as on-chain.
fn fee_amount(amount: u128, bps: u128) -> Option<u128> {
    amount
        .checked_mul(bps)?
        .checked_add(BPS_DENOMINATOR - 1)
        .map(|v| v / BPS_DENOMINATOR)
}

/// `a * b / c` floored.
fn mul_div(a: u128, b: u128, c: u128) -> Option<u128> {
    a.checked_mul(b)?.checked_div(c)
}

/// Venue fee in whole bps. Tiers are integral bps by construction; a
/// fee >= 100% fails closed (unquotable).
fn fee_bps(bps: f64) -> Option<u128> {
    if !bps.is_finite() || bps < 0.0 {
        return None;
    }
    let rounded = bps.round();
    if rounded >= BPS_DENOMINATOR as f64 {
        return None;
    }
    Some(rounded as u128)
}

impl CurveReserves {
    /// Base units per whole token (`None` past 19 decimals).
    fn scale(&self) -> Option<u64> {
        10u64.checked_pow(u32::from(self.decimals))
    }

    /// Tokens out for an exact SOL budget, fee-inclusive. Mirrors
    /// `executor::quote::buy_tokens_for_exact_in` (single total-fee
    /// ceil). `None` when the curve cannot absorb the ticket: drained
    /// reserves, or a fill that would consume every remaining real
    /// token — the chain refuses that buy, so paper must too.
    pub fn buy_tokens_base(&self, sol_lamports: u64, fee_bps_per_side: f64) -> Option<u64> {
        if sol_lamports == 0 || self.vt_base == 0 {
            return Some(0);
        }
        if self.vq_lamports == 0 {
            return None;
        }
        let bps = fee_bps(fee_bps_per_side)?;
        let sol = u128::from(sol_lamports);
        let mut net = sol * BPS_DENOMINATOR / (BPS_DENOMINATOR + bps);
        if net == 0 {
            return Some(0);
        }
        let fees = fee_amount(net, bps)?;
        if net + fees > sol {
            net = net.checked_sub(net + fees - sol).filter(|n| *n > 0)?;
        }
        let tokens = mul_div(
            net.checked_sub(1)?,
            u128::from(self.vt_base),
            u128::from(self.vq_lamports) + (net - 1),
        )?;
        if tokens >= u128::from(self.real_tokens) {
            return None;
        }
        u64::try_from(tokens).ok()
    }

    /// Fee-inclusive cost of `tokens_base`. Mirrors
    /// `executor::quote::buy_cost_incl_fees`.
    pub fn buy_cost_lamports(&self, tokens_base: u64, fee_bps_per_side: f64) -> Option<u64> {
        if tokens_base == 0 {
            return Some(0);
        }
        if self.vt_base == 0 || self.vq_lamports == 0 {
            return None;
        }
        if u128::from(tokens_base) > u128::from(self.real_tokens) {
            return None;
        }
        let bps = fee_bps(fee_bps_per_side)?;
        let denom = u128::from(self.vt_base)
            .checked_sub(u128::from(tokens_base))
            .filter(|d| *d > 0)?;
        let cost = mul_div(u128::from(tokens_base), u128::from(self.vq_lamports), denom)?
            .checked_add(1)?;
        let total = cost.checked_add(fee_amount(cost, bps)?)?;
        u64::try_from(total).ok()
    }

    /// Net SOL proceeds for `tokens_base`, fees deducted. Mirrors
    /// `executor::quote::sell_out_after_fees`. Sells always absorb
    /// (bounded impact); a drained curve has no price at all (`None`).
    pub fn sell_net_lamports(&self, tokens_base: u64, fee_bps_per_side: f64) -> Option<u64> {
        if tokens_base == 0 {
            return Some(0);
        }
        if self.vt_base == 0 || self.vq_lamports == 0 {
            return None;
        }
        let bps = fee_bps(fee_bps_per_side)?;
        let gross = mul_div(
            u128::from(tokens_base),
            u128::from(self.vq_lamports),
            u128::from(self.vt_base).checked_add(u128::from(tokens_base))?,
        )?;
        let net = gross.checked_sub(fee_amount(gross, bps)?)?;
        if net == 0 {
            return None;
        }
        u64::try_from(net).ok()
    }

    /// Exact fee-inclusive entry for a USD ticket: the whole tokens the
    /// ticket buys at the curve's cost for that (floored) size, and
    /// their price. Never spends more than the ticket.
    pub fn buy_fill(
        &self,
        notional_usd: f64,
        fee_bps_per_side: f64,
    ) -> Result<EntryFill, EntryFillError> {
        if !notional_usd.is_finite() || notional_usd <= 0.0 {
            return Err(EntryFillError::Unpriceable);
        }
        if !self.sol_usd.is_finite() || self.sol_usd <= 0.0 {
            return Err(EntryFillError::Unpriceable);
        }
        let scale = self.scale().ok_or(EntryFillError::Unpriceable)?;
        let lamports = (notional_usd / self.sol_usd * LAMPORTS_PER_SOL).floor();
        if !lamports.is_finite() || lamports <= 0.0 || lamports >= u64::MAX as f64 {
            return Err(EntryFillError::Unpriceable);
        }
        let tokens_base = self
            .buy_tokens_base(lamports as u64, fee_bps_per_side)
            .ok_or(EntryFillError::Exhausted)?;
        let quantity = tokens_base / scale;
        if quantity == 0 {
            return Err(EntryFillError::Dust);
        }
        // The book holds whole tokens: cost the floored size exactly
        // (the chain charges cost(tokens), never the nominal ticket).
        let tokens_used = quantity
            .checked_mul(scale)
            .ok_or(EntryFillError::Unpriceable)?;
        let cost = self
            .buy_cost_lamports(tokens_used, fee_bps_per_side)
            .ok_or(EntryFillError::Exhausted)?;
        let price = cost as f64 / LAMPORTS_PER_SOL * self.sol_usd / quantity as f64;
        if !price.is_finite() || price <= 0.0 {
            return Err(EntryFillError::Unpriceable);
        }
        Ok(EntryFill { quantity, price })
    }

    /// Exact fee-deducted exit price for `quantity` whole tokens.
    /// `None` when the curve has no usable reserves.
    pub fn sell_fill_price(&self, quantity: u64, fee_bps_per_side: f64) -> Option<f64> {
        if quantity == 0 || !self.sol_usd.is_finite() || self.sol_usd <= 0.0 {
            return None;
        }
        let scale = self.scale()?;
        let tokens_base = quantity.checked_mul(scale)?;
        let net = self.sell_net_lamports(tokens_base, fee_bps_per_side)?;
        let price = net as f64 / LAMPORTS_PER_SOL * self.sol_usd / quantity as f64;
        if price.is_finite() && price > 0.0 {
            Some(price)
        } else {
            None
        }
    }
}

/// Opposite side.
fn flip_side(side: Side) -> Side {
    match side {
        Side::Long => Side::Short,
        Side::Short => Side::Long,
    }
}

/// Execution price for paper fills.
///
/// Two fee layers, never confused: `fee_bps` commission on entry/exit
/// notionals (see `PaperPortfolio::new`) PLUS the venue fee already
/// embedded in fill prices. Setting `fee_bps = 125` on the default model
/// double-counts the venue leg.
///
/// The infallible helpers (`slip_fraction*`, `fill_price`) are capped
/// estimators for sizing guards (live slippage ceilings/floors are
/// protection, not fills): past the cap they understate true cost by
/// construction, so they must never settle a book. Execution paths
/// must use the `try_*` variants, which price uncapped true impact
/// and return `None` instead of printing a fictitious fill when the
/// curve cannot absorb the size (buy-side exhaustion).
#[derive(Debug, Clone, Copy)]
pub enum FillModel {
    /// Fill at quote (tests only).
    Exact,
    /// Curve cost: impact + venue fee, capped.
    /// Buys `s/(1-s)+fee`, sells `s/(1+s)+fee`.
    /// `s` = notional/liquidity, `fee` = bps/10_000.
    /// Default 125 bps/side; round trip ~2.47% (`1 - (1-f)/(1+f)`:
    /// buy pays the fee on top, sell nets it underneath).
    BondingCurve {
        /// Venue fee per side, bps.
        fee_bps_per_side: f64,
        /// Max slip fraction.
        max_slip_frac: f64,
    },
}

impl Default for FillModel {
    fn default() -> Self {
        Self::BondingCurve {
            fee_bps_per_side: STANDARD_VENUE_FEE_BPS,
            max_slip_frac: 0.25,
        }
    }
}

impl FillModel {
    /// Venue fee in bps.
    pub fn venue_fee_bps(&self) -> f64 {
        match *self {
            FillModel::Exact => 0.0,
            FillModel::BondingCurve {
                fee_bps_per_side, ..
            } => fee_bps_per_side.max(0.0),
        }
    }

    /// Same model with new fee.
    pub fn with_venue_fee_bps(&self, fee_bps_per_side: f64) -> Self {
        match *self {
            FillModel::Exact => FillModel::Exact,
            FillModel::BondingCurve { max_slip_frac, .. } => FillModel::BondingCurve {
                fee_bps_per_side: fee_bps_per_side.max(0.0),
                max_slip_frac,
            },
        }
    }

    /// One-side cost, capped.
    ///
    /// Capped estimator for sizing guards, NOT an execution fill: a buy
    /// past the curve (`notional >= liquidity`) clamps at `max_slip_frac`
    /// instead of failing. Use `try_slip_fraction_for_side` to price.
    pub fn slip_fraction(&self, side: Side, notional_usd: f64, liquidity_usd: f64) -> f64 {
        self.slip_fraction_for_side(side, notional_usd, liquidity_usd)
    }

    /// Fallible one-side cost, UNCAPPED. `None` when the price is unknowable:
    /// bad notional, non-finite/negative liquidity, or a buy whose size
    /// exhausts the curve (`notional >= liquidity`, where the
    /// constant-product impact `s/(1-s)` is unbounded). Zero liquidity
    /// prices fee-only (synthetic-grade: the caller decides whether that
    /// book is headline-eligible). Sells always absorb (bounded impact
    /// `s/(1+s)`), so a whale Long exit prices near zero instead of a
    /// fictitious capped fill. Unlike `slip_fraction_for_side`, the
    /// 25% cap does NOT apply here: the cap lives only in the
    /// infallible sizing estimators, never in execution pricing.
    pub fn try_slip_fraction_for_side(
        &self,
        side: Side,
        notional_usd: f64,
        liquidity_usd: f64,
    ) -> Option<f64> {
        if matches!(*self, FillModel::Exact) {
            return Some(0.0);
        }
        if !notional_usd.is_finite() || notional_usd <= 0.0 {
            return None;
        }
        if !liquidity_usd.is_finite() || liquidity_usd < 0.0 {
            return None;
        }
        let fee = match *self {
            FillModel::Exact => 0.0,
            FillModel::BondingCurve { fee_bps_per_side, .. } => fee_bps_per_side.max(0.0) / 10_000.0,
        };
        if liquidity_usd == 0.0 {
            return Some(fee);
        }
        let size = (notional_usd / liquidity_usd).max(0.0);
        let impact = match side {
            Side::Long if size >= 1.0 => return None,
            Side::Long => size / (1.0 - size),
            Side::Short => size / (1.0 + size),
        };
        // Deliberately uncapped (see doc): execution prices true impact.
        Some((impact + fee).max(0.0))
    }

    /// `try_slip_fraction_for_side` with the position side implied by `side`.
    pub fn try_slip_fraction(
        &self,
        side: Side,
        notional_usd: f64,
        liquidity_usd: f64,
    ) -> Option<f64> {
        self.try_slip_fraction_for_side(side, notional_usd, liquidity_usd)
    }

    pub fn slip_fraction_for_side(&self, side: Side, notional_usd: f64, liquidity_usd: f64) -> f64 {
        if let FillModel::BondingCurve {
            fee_bps_per_side,
            max_slip_frac,
        } = *self
        {
            debug_assert!(
                fee_bps_per_side >= 0.0,
                "negative venue fee silently zeroes costs"
            );
            debug_assert!(
                max_slip_frac >= 0.0,
                "negative slip cap silently zeroes costs"
            );
        }
        let fee = match *self {
            FillModel::Exact => 0.0,
            FillModel::BondingCurve { fee_bps_per_side, .. } => fee_bps_per_side.max(0.0) / 10_000.0,
        };
        let cap = match *self {
            FillModel::Exact => 0.0,
            FillModel::BondingCurve { max_slip_frac, .. } => max_slip_frac.max(0.0),
        };
        let size = if liquidity_usd.is_finite() && liquidity_usd > 0.0 {
            (notional_usd / liquidity_usd).max(0.0)
        } else {
            0.0
        };
        let impact = match *self {
            FillModel::Exact => 0.0,
            FillModel::BondingCurve { .. } => match side {
                Side::Long => {
                    if size >= 1.0 {
                        f64::INFINITY
                    } else {
                        size / (1.0 - size)
                    }
                }
                Side::Short => size / (1.0 + size),
            },
        };
        (impact + fee).clamp(0.0, cap)
    }
    /// Quote to execution price.
    ///
    /// Capped estimator (see `slip_fraction`): never fails, even past the
    /// curve. Execution paths must use `try_fill_price`.
    pub fn fill_price(&self, side: Side, quote: f64, notional_usd: f64, liquidity_usd: f64) -> f64 {
        if !quote.is_finite() || quote <= 0.0 || !notional_usd.is_finite() || notional_usd <= 0.0 {
            return quote;
        }
        let slip = self.slip_fraction_for_side(side, notional_usd, liquidity_usd);
        match side {
            Side::Long => quote * (1.0 + slip),
            Side::Short => quote * (1.0 - slip).max(0.0),
        }
    }

    /// Fallible quote to execution price (uncapped true impact).
    /// `None` exactly when `try_slip_fraction_for_side` is `None`
    /// (bad inputs, unknown liquidity, or curve exhaustion on buys).
    /// A giant Long exit (sell side) prices near zero, never at a
    /// capped `0.75 * quote` fiction.
    pub fn try_fill_price(
        &self,
        side: Side,
        quote: f64,
        notional_usd: f64,
        liquidity_usd: f64,
    ) -> Option<f64> {
        if !quote.is_finite() || quote <= 0.0 {
            return None;
        }
        let slip = self.try_slip_fraction_for_side(side, notional_usd, liquidity_usd)?;
        Some(match side {
            Side::Long => quote * (1.0 + slip),
            Side::Short => quote * (1.0 - slip).max(0.0),
        })
    }

    /// Entry fill for `side`: exact curve math from the mark's reserves
    /// when the tick carried them (Long buys the curve), else the proxy
    /// basis (`try_fill_price`). Size-aware either way: the ticket buys
    /// the tokens the curve actually yields, fees inside the price.
    ///
    /// `Dust` = the size floors to zero whole tokens; `Exhausted` = the
    /// curve cannot absorb the ticket (drained or depleted); neither is
    /// ever booked as a fill.
    pub fn entry_fill(
        &self,
        side: Side,
        mark: &Mark,
        notional_usd: f64,
    ) -> Result<EntryFill, EntryFillError> {
        if !notional_usd.is_finite() || notional_usd <= 0.0 {
            return Err(EntryFillError::Unpriceable);
        }
        if let (
            Side::Long,
            FillModel::BondingCurve {
                fee_bps_per_side, ..
            },
            Some(curve),
        ) = (side, *self, mark.curve)
        {
            return curve.buy_fill(notional_usd, fee_bps_per_side);
        }
        let price = self
            .try_fill_price(side, mark.price, notional_usd, mark.liquidity_usd)
            .ok_or(EntryFillError::Exhausted)?;
        let quantity = (notional_usd / price).floor();
        if !quantity.is_finite() || quantity < 1.0 || quantity >= u64::MAX as f64 {
            return Err(EntryFillError::Dust);
        }
        Ok(EntryFill {
            quantity: quantity as u64,
            price,
        })
    }

    /// Exit price for `side` (a Long exits by selling into the curve):
    /// exact fee-deducted curve price from the mark's reserves when the
    /// tick carried them, else the proxy basis. `None` when unpriceable
    /// (drained curve, unknown liquidity, bad quote) — the caller holds
    /// rather than booking a fictitious exit.
    pub fn exit_price(&self, side: Side, mark: &Mark, quantity: u64) -> Option<f64> {
        if quantity == 0 || !mark.price.is_finite() || mark.price <= 0.0 {
            return None;
        }
        if let (
            Side::Long,
            FillModel::BondingCurve {
                fee_bps_per_side, ..
            },
            Some(curve),
        ) = (side, *self, mark.curve)
        {
            return curve.sell_fill_price(quantity, fee_bps_per_side);
        }
        let exit_notional = quantity as f64 * mark.price;
        let slip =
            self.try_slip_fraction_for_side(flip_side(side), exit_notional, mark.liquidity_usd)?;
        Some(match side {
            Side::Long => mark.price * (1.0 - slip).max(0.0),
            Side::Short => mark.price * (1.0 + slip),
        })
    }
}

/// Signed return since entry, percent.
pub fn move_percent(side: Side, entry_price: f64, price: f64) -> f64 {
    if !entry_price.is_finite() || entry_price <= 0.0 || !price.is_finite() {
        return 0.0;
    }
    match side {
        Side::Long => (price - entry_price) / entry_price * 100.0,
        Side::Short => (entry_price - price) / entry_price * 100.0,
    }
}

/// Exit check against the realizable exit price (fee-aware fill), not
/// the gross mark: SL must fire on what the exit actually nets, and TP
/// must not book phantom edge the fill never pays. Timeout still fires
/// on unknowable fills; anything else holds (settlement prices the
/// same fill, so triggers and books agree by construction).
pub fn check_exit_realizable(
    fill: &FillModel,
    position: &mut Position,
    mark: &Mark,
    now_secs: u64,
) -> Option<ExitReason> {
    // Timeout ignores pricing.
    if position.timeout_seconds > 0
        && now_secs.saturating_sub(position.opened_at) >= position.timeout_seconds
    {
        return Some(ExitReason::Timeout);
    }
    let price = fill.exit_price(position.side, mark, position.quantity)?;
    check_exit(position, price, now_secs)
}

/// Exit check: timeout, TP, SL, trailing.
pub fn check_exit(position: &mut Position, price: f64, now_secs: u64) -> Option<ExitReason> {
    // Timeout ignores bad prices.
    if position.timeout_seconds > 0
        && now_secs.saturating_sub(position.opened_at) >= position.timeout_seconds
    {
        return Some(ExitReason::Timeout);
    }
    if !price.is_finite() || price <= 0.0 {
        return None;
    }

    let moved = move_percent(position.side, position.entry_price, price);

    if moved >= position.tp_percent {
        return Some(ExitReason::TakeProfit);
    }
    if moved <= position.sl_percent {
        return Some(ExitReason::StopLoss);
    }

    if position.trailing_stop && position.trail_percent > 0.0 {
        let improved = match position.side {
            Side::Long => price > position.best_price,
            Side::Short => price < position.best_price,
        };
        if improved {
            position.best_price = price;
        }
        let giveback = move_percent(position.side, position.best_price, price);
        if giveback <= -position.trail_percent {
            return Some(ExitReason::TrailingStop);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pump.fun launch scale: 30 SOL virtual / 1.073e15 virtual tokens /
    /// 793.1e12 real tokens, 6 decimals, SOL $200.
    fn launch_curve() -> CurveReserves {
        CurveReserves {
            vq_lamports: 30_000_000_000,
            vt_base: 1_073_000_000_000_000,
            real_tokens: 793_100_000_000_000,
            decimals: 6,
            sol_usd: 200.0,
        }
    }

    #[test]
    fn curve_buy_prices_state_and_size() {
        let curve = launch_curve();
        let fill = curve.buy_fill(10.0, 125.0).expect("priced");
        // Hand-computed from the chain formula (executor::quote):
        // net = 50e6*10_000/10_125 = 49_382_716, fee = 617_284,
        // tokens = (net-1)*vt/(vq+net-1) = 1_763_352_468_753 base units.
        assert_eq!(fill.quantity, 1_763_352);
        // Cost is the curve's fee-inclusive price for the floored size,
        // never the nominal ticket (49_999_986 of 50_000_000 lamports).
        let invested = fill.price * fill.quantity as f64;
        assert!(invested <= 10.0, "invested={invested}");
        assert!(invested > 9.999, "invested={invested}");
        // Above spot: impact + fee.
        assert!(
            fill.price > 5.591_798_695_246_971e-6,
            "price={}",
            fill.price
        );
    }

    #[test]
    fn curve_sell_nets_size_impact_and_fee() {
        let curve = launch_curve();
        let spot = 5.591_798_695_246_971e-6;
        let price = curve.sell_fill_price(1_763_352, 125.0).expect("priced");
        // Hand-computed: gross = 49_220_658, fee = 615_259, net = 48_605_399.
        assert!(
            (price - 5.512_841_338_541_596e-6).abs() < 1e-15,
            "price={price}"
        );
        assert!(price < spot, "sell must net under the mark");
        // A bigger size nets a deeper discount (impact is size-aware).
        let deeper = curve.sell_fill_price(176_335_200, 125.0).expect("priced");
        assert!(deeper < price, "deeper={deeper} price={price}");
    }

    #[test]
    fn curve_buy_rejects_tickets_the_curve_cannot_absorb() {
        let curve = launch_curve();
        assert_eq!(
            curve.buy_fill(1e12, 125.0).unwrap_err(),
            EntryFillError::Exhausted
        );
        // Drained reserves are unpriceable on both sides.
        let drained = CurveReserves {
            vq_lamports: 0,
            ..launch_curve()
        };
        assert_eq!(
            drained.buy_fill(10.0, 125.0).unwrap_err(),
            EntryFillError::Exhausted
        );
        assert_eq!(drained.sell_fill_price(1_000_000, 125.0), None);
    }

    #[test]
    fn curve_fill_floors_sub_token_dust() {
        // 1000 SOL virtual against 1 whole token: a $0.000001 ticket
        // buys less than one whole token, so it cannot open.
        let curve = CurveReserves {
            vq_lamports: 1_000_000_000_000,
            vt_base: 1_000_000,
            real_tokens: 1_000_000_000,
            decimals: 6,
            sol_usd: 200.0,
        };
        assert_eq!(curve.buy_fill(1e-6, 0.0).unwrap_err(), EntryFillError::Dust);
    }

    #[test]
    fn exact_curve_state_overrides_the_proxy_basis() {
        let fill = FillModel::default();
        // Liquidity $1: the proxy would refuse a $10 ticket outright.
        let mark = Mark::new(5.591_798_695_246_971e-6, 1.0).with_curve(launch_curve());
        let entry = fill
            .entry_fill(Side::Long, &mark, 10.0)
            .expect("exact priced");
        assert_eq!(entry.quantity, 1_763_352);
        let exit = fill
            .exit_price(Side::Long, &mark, entry.quantity)
            .expect("priced");
        assert!(exit < mark.price, "exit={exit}");
        // Without reserves the same mark is exhausted, as before.
        let proxy_only = Mark::new(mark.price, 1.0);
        assert_eq!(
            fill.entry_fill(Side::Long, &proxy_only, 10.0),
            Err(EntryFillError::Exhausted)
        );
    }

    #[test]
    fn proxy_entry_fill_matches_try_fill_price() {
        let fill = FillModel::default();
        let mark = Mark::new(1.0, 10_000.0);
        let got = fill.entry_fill(Side::Long, &mark, 100.0).expect("priced");
        let price = fill
            .try_fill_price(Side::Long, 1.0, 100.0, 10_000.0)
            .expect("priced");
        assert_eq!(got.price, price);
        assert_eq!(got.quantity, (100.0 / price).floor() as u64);
    }

    fn long(tp: f64, sl: f64) -> Position {
        Position {
            id: 1,
            mint: "MINT".to_string(),
            side: Side::Long,
            entry_price: 1.0,
            quantity: 100,
            entry_notional: 100.0,
            entry_fee: 0.0,
            tp_percent: tp,
            sl_percent: sl,
            trail_percent: 5.0,
            trailing_stop: false,
            best_price: 1.0,
            opened_at: 1000,
            timeout_seconds: 0,
            moonbag_percent: 0.0,
        }
    }

    #[test]
    fn exact_model_fills_at_quote() {
        assert_eq!(
            FillModel::Exact.fill_price(Side::Long, 2.0, 500.0, 10_000.0),
            2.0
        );
    }

    #[test]
    fn impact_is_fee_plus_size_and_hurts_both_sides() {
        let model = FillModel::BondingCurve {
            fee_bps_per_side: 125.0,
            max_slip_frac: 0.25,
        };
        let buy = model.fill_price(Side::Long, 1.0, 100.0, 10_000.0);
        assert!((buy - 1.022_601_010_101_01).abs() < 1e-12, "buy={buy}");
        let sell = model.fill_price(Side::Short, 1.0, 100.0, 10_000.0);
        assert!((sell - 0.977_599_009_900_99).abs() < 1e-12, "sell={sell}");
        let buy10 = model.slip_fraction_for_side(Side::Long, 1_000.0, 10_000.0);
        assert!(
            (buy10 - (0.1 / 0.9 + 0.0125)).abs() < 1e-12,
            "buy10={buy10}"
        );
        let sell10 = model.slip_fraction_for_side(Side::Short, 1_000.0, 10_000.0);
        assert!(
            (sell10 - (0.1 / 1.1 + 0.0125)).abs() < 1e-12,
            "sell10={sell10}"
        );
        let capped = model.fill_price(Side::Long, 1.0, 10_000_000.0, 10_000.0);
        assert!((capped - 1.25).abs() < 1e-12, "capped={capped}");
        let capped_sell = model.fill_price(Side::Short, 1.0, 10_000_000.0, 10_000.0);
        assert!(
            (capped_sell - 0.75).abs() < 1e-12,
            "capped_sell={capped_sell}"
        );
    }

    #[test]
    fn realistic_ticket_costs_match_live_band() {
        let model = FillModel::default();
        let buy = model.fill_price(Side::Long, 1.0, 10.0, 4_500.0);
        let slip = buy - 1.0;
        assert!((slip - 0.0147).abs() < 1e-4, "slip={slip}");
        let qty = 10.0 / buy;
        let sell_px = model.fill_price(Side::Short, 1.0, qty, 4_500.0);
        let loss = 10.0 - qty * sell_px;
        assert!((loss - 0.29).abs() < 0.01, "loss={loss}");
    }

    #[test]
    fn degenerate_inputs_fall_back_to_quote() {
        let model = FillModel::default();
        assert_eq!(model.fill_price(Side::Long, 0.0, 100.0, 10_000.0), 0.0);
        assert!((model.fill_price(Side::Long, 1.0, 100.0, 0.0) - 1.0125).abs() < 1e-12);
        assert!(
            !model
                .fill_price(Side::Long, f64::NAN, 100.0, 10_000.0)
                .is_finite()
        );
    }

    #[test]
    fn try_execution_pricing_is_uncapped() {
        // Execution prints true impact: the 25% cap binds only the
        // infallible sizing estimators, never a booked fill. At
        // 125bps/side the sell cap would bind at s ~= 0.31; the
        // execution leg prices straight through it.
        let model = FillModel::default();
        // In-band sell (s = 0.5): true 0.5/1.5 + fee, not 0.25.
        let slip = model
            .try_slip_fraction(Side::Short, 5_000.0, 10_000.0)
            .expect("sells always absorb");
        assert!((slip - (0.5 / 1.5 + 0.0125)).abs() < 1e-12, "slip={slip}");
        // In-band buy (s = 0.5): true 0.5/0.5 + fee, not 0.25.
        let buy_slip = model
            .try_slip_fraction(Side::Long, 5_000.0, 10_000.0)
            .expect("s < 1 prices");
        assert!((buy_slip - (1.0 + 0.0125)).abs() < 1e-12, "buy_slip={buy_slip}");
        // Whale Long exit (s = 1000): impact ~= 1.0, fill floors at 0.
        let whale = model
            .try_fill_price(Side::Short, 1.0, 10_000_000.0, 10_000.0)
            .expect("bounded impact prices");
        assert_eq!(whale, 0.0, "whale exit must print ~0, not 0.75");
        // Buy-side exhaustion still rejects (unbounded impact).
        assert_eq!(model.try_fill_price(Side::Long, 1.0, 10_000_000.0, 10_000.0), None);
        // Zero liquidity stays fee-only (synthetic-grade, caller-gated).
        assert_eq!(
            model.try_slip_fraction(Side::Short, 100.0, 0.0),
            Some(0.0125)
        );
    }

    #[test]
    fn tp_sl_trigger() {
        let mut p = long(75.0, -8.0);
        assert_eq!(check_exit(&mut p, 1.75, 1001), Some(ExitReason::TakeProfit));
        assert_eq!(check_exit(&mut p, 0.91, 1001), Some(ExitReason::StopLoss));
        assert_eq!(check_exit(&mut p, 1.10, 1001), None);
    }

    #[test]
    fn realizable_triggers_agree_with_settlement() {
        // Default 125bps/side. Gross +76% is only +73.8% net: TP holds
        // on the realizable basis (no phantom edge).
        let fill = FillModel::default();
        let mut p = long(75.0, -8.0);
        assert_eq!(
            check_exit_realizable(&fill, &mut p, &Mark::new(1.76, 1e9), 1001),
            None
        );
        // Gross -7.5% is -8.7% net: SL fires a tick earlier than gross.
        let mut p = long(75.0, -8.0);
        assert_eq!(
            check_exit_realizable(&fill, &mut p, &Mark::new(0.925, 1e9), 1001),
            Some(ExitReason::StopLoss)
        );
        // Timeout still fires when the fill is unknowable.
        let mut p = long(75.0, -8.0);
        p.timeout_seconds = 60;
        assert_eq!(
            check_exit_realizable(&fill, &mut p, &Mark::new(f64::NAN, 1e9), 1060),
            Some(ExitReason::Timeout)
        );
        // ...but nothing else does.
        let mut p = long(75.0, -8.0);
        assert_eq!(
            check_exit_realizable(&fill, &mut p, &Mark::new(f64::NAN, 1e9), 1001),
            None
        );
    }

    #[test]
    fn trailing_ratchets_and_fires() {
        let mut p = long(10_000.0, -10_000.0);
        p.trailing_stop = true;
        assert_eq!(check_exit(&mut p, 1.5, 1001), None);
        assert_eq!(p.best_price, 1.5);
        assert_eq!(check_exit(&mut p, 1.44, 1002), None);
        assert_eq!(
            check_exit(&mut p, 1.41, 1003),
            Some(ExitReason::TrailingStop)
        );
    }

    #[test]
    fn timeout_fires_first() {
        let mut p = long(75.0, -8.0);
        p.timeout_seconds = 60;
        assert_eq!(check_exit(&mut p, 5.0, 1060), Some(ExitReason::Timeout));
    }

    #[test]
    fn timeout_fires_on_degenerate_price() {
        let mut p = long(75.0, -8.0);
        p.timeout_seconds = 60;
        assert_eq!(check_exit(&mut p, 0.0, 1060), Some(ExitReason::Timeout));
        let mut p = long(75.0, -8.0);
        p.timeout_seconds = 60;
        assert_eq!(check_exit(&mut p, f64::NAN, 1060), Some(ExitReason::Timeout));
    }

    #[test]
    fn short_side_mirrors() {
        let mut p = long(75.0, -8.0);
        p.side = Side::Short;
        assert_eq!(check_exit(&mut p, 0.25, 1001), Some(ExitReason::TakeProfit));
        assert_eq!(check_exit(&mut p, 1.08, 1001), Some(ExitReason::StopLoss));
    }
}

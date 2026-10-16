//! Slippage fills and exit triggers.

use crate::types::{ExitReason, Position, Side};

/// Standard venue fee per side, bps. Live-launch schedule total
/// (95 + 30); custom-tier mints override per mint.
pub const STANDARD_VENUE_FEE_BPS: f64 = 125.0;

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
    /// Default 125 bps/side; round trip ~2.53%.
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
    quote: f64,
    exit_notional_usd: f64,
    liquidity_usd: f64,
    now_secs: u64,
) -> Option<ExitReason> {
    // Timeout ignores pricing.
    if position.timeout_seconds > 0
        && now_secs.saturating_sub(position.opened_at) >= position.timeout_seconds
    {
        return Some(ExitReason::Timeout);
    }
    let flip = match position.side {
        Side::Long => Side::Short,
        Side::Short => Side::Long,
    };
    let price = fill.try_fill_price(flip, quote, exit_notional_usd, liquidity_usd)?;
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
            check_exit_realizable(&fill, &mut p, 1.76, 176.0, 1e9, 1001),
            None
        );
        // Gross -7.5% is -8.7% net: SL fires a tick earlier than gross.
        let mut p = long(75.0, -8.0);
        assert_eq!(
            check_exit_realizable(&fill, &mut p, 0.925, 92.5, 1e9, 1001),
            Some(ExitReason::StopLoss)
        );
        // Timeout still fires when the fill is unknowable.
        let mut p = long(75.0, -8.0);
        p.timeout_seconds = 60;
        assert_eq!(
            check_exit_realizable(&fill, &mut p, f64::NAN, 100.0, 1e9, 1060),
            Some(ExitReason::Timeout)
        );
        // ...but nothing else does.
        let mut p = long(75.0, -8.0);
        assert_eq!(
            check_exit_realizable(&fill, &mut p, f64::NAN, 100.0, 1e9, 1001),
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

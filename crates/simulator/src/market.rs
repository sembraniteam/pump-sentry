//! Slippage fills and exit triggers.

use crate::types::{ExitReason, Position, Side};

/// Execution price for paper fills.
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
            fee_bps_per_side: 125.0,
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
    pub fn slip_fraction(&self, side: Side, notional_usd: f64, liquidity_usd: f64) -> f64 {
        self.slip_fraction_for_side(side, notional_usd, liquidity_usd)
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
            quantity: 100.0,
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
    fn tp_sl_trigger() {
        let mut p = long(75.0, -8.0);
        assert_eq!(check_exit(&mut p, 1.75, 1001), Some(ExitReason::TakeProfit));
        assert_eq!(check_exit(&mut p, 0.91, 1001), Some(ExitReason::StopLoss));
        assert_eq!(check_exit(&mut p, 1.10, 1001), None);
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

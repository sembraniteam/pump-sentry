//! Market microstructure helpers: slippage fills and exit triggers.
//!
//! [`FillModel`] turns a quote into an execution price given trade size
//! vs available liquidity. [`check_exit`] evaluates TP / SL / trailing /
//! timeout against the current price and mutates trailing state.

use crate::types::{ExitReason, Position, Side};

/// Execution-price model for paper fills.
///
/// Quotes on thin bonding curves are never the fill: every trade pays
/// the venue fee plus constant-product price impact against its own size.
#[derive(Debug, Clone, Copy)]
pub enum FillModel {
    /// Fill exactly at the quote (for benchmarks / tests).
    Exact,
    /// Bonding-curve cost: `slip_frac = size_frac + fee_frac`, where
    /// `size_frac = notional_usd / liquidity_usd` is the exact
    /// first-order constant-product impact (dx/x per side) and
    /// `fee_frac = fee_bps_per_side / 10_000` is the venue fee —
    /// 125 bps on Pump.fun (0.95% protocol + 0.30% creator), identical on
    /// buys and sells, so a flat round trip costs ~2.53% before impact.
    /// Clamped to `max_slip_frac`. Calibrated 2026-10-11 against
    /// pump.fun/docs/fees after the old 50bps/1% linear model understated
    /// a $10-into-$4500 fill by ~13x (live round trips measured 2–4%).
    BondingCurve {
        /// Venue fee per side in basis points (125.0 on Pump.fun).
        fee_bps_per_side: f64,
        /// Hard cap on the slip fraction (e.g. `0.25` = 25%).
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
    /// Convert a quote into an execution price.
    ///
    /// Buys fill *above* the quote, sells *below* it. Never returns NaN,
    /// zero, or negative: degenerate inputs fall back to the quote.
    pub fn fill_price(&self, side: Side, quote: f64, notional_usd: f64, liquidity_usd: f64) -> f64 {
        if !quote.is_finite() || quote <= 0.0 || !notional_usd.is_finite() || notional_usd <= 0.0 {
            return quote;
        }
        let slip = match *self {
            FillModel::Exact => 0.0,
            FillModel::BondingCurve {
                fee_bps_per_side,
                max_slip_frac,
            } => {
                if !liquidity_usd.is_finite() || liquidity_usd <= 0.0 {
                    return quote;
                }
                let size_frac = notional_usd / liquidity_usd;
                let fee_frac = (fee_bps_per_side.max(0.0) / 10_000.0).max(0.0);
                (size_frac + fee_frac).clamp(0.0, max_slip_frac.max(0.0))
            }
        };
        match side {
            Side::Long => quote * (1.0 + slip),
            Side::Short => quote * (1.0 - slip).max(0.0),
        }
    }
}

/// Signed return since entry: `(price - entry) / entry` for longs,
/// mirrored for shorts. Returns `0.0` on degenerate entry prices.
pub fn move_percent(side: Side, entry_price: f64, price: f64) -> f64 {
    if !entry_price.is_finite() || entry_price <= 0.0 || !price.is_finite() {
        return 0.0;
    }
    match side {
        Side::Long => (price - entry_price) / entry_price * 100.0,
        Side::Short => (entry_price - price) / entry_price * 100.0,
    }
}

/// Evaluate exit triggers for a position at `price` / `now_secs`.
///
/// Order: timeout → take-profit → stop-loss → trailing-stop. On a
/// favorable move the trailing extreme (`best_price`) ratchets; it never
/// moves against the position. Returns `None` when no trigger fires.
///
/// Callers close the position themselves via
/// [`PaperPortfolio::close_position`](crate::PaperPortfolio::close_position).
pub fn check_exit(position: &mut Position, price: f64, now_secs: u64) -> Option<ExitReason> {
    if !price.is_finite() || price <= 0.0 {
        return None;
    }

    if position.timeout_seconds > 0
        && now_secs.saturating_sub(position.opened_at) >= position.timeout_seconds
    {
        return Some(ExitReason::Timeout);
    }

    let moved = move_percent(position.side, position.entry_price, price);

    if moved >= position.tp_percent {
        return Some(ExitReason::TakeProfit);
    }
    if moved <= position.sl_percent {
        return Some(ExitReason::StopLoss);
    }

    if position.trailing_stop && position.trail_percent > 0.0 {
        // Ratchet the extreme on favorable moves only.
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
        // 1% of liquidity: 1% impact + 1.25% fee = 2.25% against.
        let buy = model.fill_price(Side::Long, 1.0, 100.0, 10_000.0);
        assert!((buy - 1.0225).abs() < 1e-12, "buy={buy}");
        let sell = model.fill_price(Side::Short, 1.0, 100.0, 10_000.0);
        assert!((sell - 0.9775).abs() < 1e-12, "sell={sell}");
        // Cap binds on huge size.
        let capped = model.fill_price(Side::Long, 1.0, 10_000_000.0, 10_000.0);
        assert!((capped - 1.25).abs() < 1e-12, "capped={capped}");
    }

    #[test]
    fn realistic_ticket_costs_match_live_band() {
        // $10 into $4500 virtual liquidity: 0.22% impact + 1.25% fee.
        let model = FillModel::default();
        let buy = model.fill_price(Side::Long, 1.0, 10.0, 4_500.0);
        let slip = buy - 1.0;
        assert!((slip - 0.0147).abs() < 1e-4, "slip={slip}");
        // Full round trip in token terms: ~2.9% gone, inside the 2–4%
        // band measured live (old model said ~0.2%).
        let qty = 10.0 / buy;
        let sell_px = model.fill_price(Side::Short, 1.0, qty, 4_500.0);
        let loss = 10.0 - qty * sell_px;
        assert!((loss - 0.29).abs() < 0.01, "loss={loss}");
    }

    #[test]
    fn degenerate_inputs_fall_back_to_quote() {
        let model = FillModel::default();
        assert_eq!(model.fill_price(Side::Long, 0.0, 100.0, 10_000.0), 0.0);
        assert_eq!(model.fill_price(Side::Long, 1.0, 100.0, 0.0), 1.0);
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
        // 4% giveback from 1.5: no fire (trail 5%).
        assert_eq!(check_exit(&mut p, 1.44, 1002), None);
        // 6% giveback: fires.
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
    fn short_side_mirrors() {
        let mut p = long(75.0, -8.0);
        p.side = Side::Short;
        assert_eq!(check_exit(&mut p, 0.25, 1001), Some(ExitReason::TakeProfit));
        assert_eq!(check_exit(&mut p, 1.08, 1001), Some(ExitReason::StopLoss));
    }
}

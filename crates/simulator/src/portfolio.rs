//! Virtual portfolio.

use crate::market::{EntryFillError, FillModel};
use crate::types::{ClosedTrade, ExitReason, Position, PositionId, Side};
use std::collections::HashMap;
use thiserror::Error;
use tracing::warn;

/// Runner backstop (24h).
pub const RUNNER_BACKSTOP_SECS: u64 = 86_400;

/// Portfolio errors.
#[derive(Error, Debug, PartialEq)]
pub enum PortfolioError {
    #[error("invalid price: {0}")]
    InvalidPrice(f64),
    #[error("invalid notional: {0}")]
    InvalidNotional(f64),
    #[error("insufficient funds: need {need:.2}, have {have:.2}")]
    InsufficientFunds {
        need: f64,
        have: f64,
    },
    #[error("position not found: {0}")]
    PositionNotFound(PositionId),
    #[error("invalid take-profit: {0}")]
    InvalidTakeProfit(f64),
    #[error("invalid stop-loss: {0}")]
    InvalidStopLoss(f64),
    #[error("invalid close fraction: {0}")]
    InvalidFraction(f64),
    #[error("invalid mint: {0:?}")]
    InvalidMint(String),
    #[error("position id space exhausted")]
    IdExhausted,
    #[error("curve cannot absorb size (notional {notional:.2} vs liquidity {liquidity:.2})")]
    CurveExhausted { notional: f64, liquidity: f64 },
}

/// Position slice being settled.
struct CloseSlice {
    id: PositionId,
    mint: String,
    side: Side,
    entry_price: f64,
    quantity: u64,
    entry_notional: f64,
    entry_fee: f64,
    opened_at: u64,
    fraction: f64,
}

impl CloseSlice {
    fn whole(position: &Position) -> Self {
        Self {
            id: position.id,
            mint: position.mint.clone(),
            side: position.side,
            entry_price: position.entry_price,
            quantity: position.quantity,
            entry_notional: position.entry_notional,
            entry_fee: position.entry_fee,
            opened_at: position.opened_at,
            fraction: 1.0,
        }
    }

    fn part(position: &Position, fraction: f64) -> Self {
        Self {
            id: position.id,
            mint: position.mint.clone(),
            side: position.side,
            entry_price: position.entry_price,
            quantity: slice_quantity(position.quantity, fraction),
            entry_notional: position.entry_notional * fraction,
            entry_fee: position.entry_fee * fraction,
            opened_at: position.opened_at,
            fraction,
        }
    }
}

/// Whole-token slice of `quantity` for a partial close: floored, never
/// rounded (overselling is unauthorized). A fraction at or above 100%
/// slices the whole position; a zero, negative, or NaN fraction slices
/// to zero (the book rejects that as dust; matches a live sell that
/// sizes to zero). Single source: the paper book and the live sell
/// mirror size every scale-out through this, so the two can never
/// disagree by a token.
pub fn slice_quantity(quantity: u64, fraction: f64) -> u64 {
    if fraction >= 1.0 {
        return quantity;
    }
    if !fraction.is_finite() || fraction <= 0.0 {
        return 0;
    }
    (quantity as f64 * fraction).floor() as u64
}

/// Finite value or zero.
fn finite_or_zero(v: f64) -> f64 {
    if v.is_finite() { v } else { 0.0 }
}

/// Mark-to-market quote for one mint. Defined in `market` (pricing
/// engine) and re-exported here for the book's public surface.
pub use crate::market::Mark;

/// Open-position inputs.
#[derive(Debug, Clone)]
pub struct OpenPositionParams<'a> {
    pub mint: &'a str,
    pub side: Side,
    /// Market state at open (spot, depth, and exact curve reserves when
    /// the tick carried them).
    pub mark: Mark,
    pub notional_usd: f64,
    pub tp_percent: f64,
    pub sl_percent: f64,
    pub trailing: bool,
    pub trail_percent: f64,
    pub timeout_seconds: u64,
    pub moonbag_percent: f64,
    pub now_secs: u64,
}

/// Accounting snapshot. `realized_pnl` is trade edge only and
/// excludes dispatch bleed — use `realized_net_pnl()` (edge minus
/// bleed) for headlines and the breaker, never this field.
///
/// `mark_value` prices open positions at the impact-free mark;
/// `realizable_value` prices them at the exit the book would actually
/// book (size impact + venue fee). The gap is the liquidation
/// discount — see [`Self::exit_discount_usd`].
#[derive(Debug, Clone)]
pub struct PortfolioSnapshot {
    pub cash: f64,
    pub locked: f64,
    pub equity: f64,
    pub realized_pnl: f64,
    pub unrealized_pnl: f64,
    /// Signed mark-to-market value of open positions (no impact/fee).
    pub mark_value: f64,
    /// Signed exit value of open positions (size impact + venue fee).
    pub realizable_value: f64,
    pub open_count: usize,
    pub closed_count: usize,
}

impl PortfolioSnapshot {
    /// Mark minus realizable: what exiting the book's full size would
    /// cost in impact and venue fees right now. Zero when nothing is
    /// open; negative for a Long book under any impact.
    pub fn exit_discount_usd(&self) -> f64 {
        self.mark_value - self.realizable_value
    }
}

/// Paper-trading portfolio.
#[derive(Debug)]
pub struct PaperPortfolio {
    starting_balance: f64,
    cash: f64,
    fee_bps: f64,
    fill: FillModel,
    /// Per-mint venue fees (bps/side).
    venue_fees: HashMap<String, f64>,
    next_id: PositionId,
    positions: HashMap<PositionId, Position>,
    history: Vec<ClosedTrade>,
    realized_sum: f64,
    win_count: usize,
    closed_total: usize,
    /// Cumulative dispatch bleed (tips/base/rent) debited via
    /// [`Self::debit_dispatch_cost`]. Kept parallel to `realized_sum`
    /// so `cash`/`equity` and headline PnL can't diverge silently.
    total_dispatch_usd: f64,
    /// Paper-mode per-leg bleed estimates (open, close), USD. Live
    /// mode debits actuals instead (see `set_paper_dispatch_legs`).
    /// Absolute lamports, not a rate: the drag as a percent falls as
    /// `NOTIONAL_USD` rises (see `paper_dispatch_legs_usd`).
    paper_dispatch_legs: (f64, f64),
}

/// Detail history cap.
pub const MAX_HISTORY_TRADES: usize = 50_000;

impl PaperPortfolio {
    /// Fund a portfolio.
    pub fn new(starting_balance_usd: f64, fee_bps: f64) -> Self {
        Self {
            starting_balance: starting_balance_usd.max(0.0),
            cash: starting_balance_usd.max(0.0),
            fee_bps: fee_bps.max(0.0),
            fill: FillModel::default(),
            venue_fees: HashMap::new(),
            next_id: 1,
            positions: HashMap::new(),
            history: Vec::new(),
            realized_sum: 0.0,
            win_count: 0,
            closed_total: 0,
            total_dispatch_usd: 0.0,
            paper_dispatch_legs: (0.0, 0.0),
        }
    }

    /// Set fill model.
    pub fn with_fill_model(mut self, fill: FillModel) -> Self {
        self.fill = fill;
        self
    }

    /// Set venue fee for one mint.
    pub fn set_venue_fee_for_mint(&mut self, mint: &str, fee_bps_per_side: f64) {
        if mint.is_empty() || !fee_bps_per_side.is_finite() || fee_bps_per_side < 0.0 {
            warn!(mint, fee = fee_bps_per_side, "ignoring invalid venue fee tier");
            return;
        }
        self.venue_fees.insert(mint.to_string(), fee_bps_per_side);
    }

    /// Drop a mint override.
    pub fn clear_venue_fee_for_mint(&mut self, mint: &str) {
        self.venue_fees.remove(mint);
    }

    /// Venue fee for one mint.
    pub fn venue_fee_for(&self, mint: &str) -> f64 {
        self.venue_fees
            .get(mint)
            .copied()
            .unwrap_or_else(|| self.fill.venue_fee_bps())
    }

    fn fill_for_mint(&self, mint: &str) -> FillModel {
        match self.venue_fees.get(mint) {
            Some(fee) => self.fill.with_venue_fee_bps(*fee),
            None => self.fill,
        }
    }

    pub fn cash(&self) -> f64 {
        self.cash
    }

    /// Debit dispatch cost. Bleeds `cash` (floored at zero) and records
    /// the cumulative bleed so headline PnL can net it (see
    /// [`Self::realized_net_pnl`]). Non-finite/non-positive costs ignored.
    pub fn debit_dispatch_cost(&mut self, cost_usd: f64) {
        if cost_usd.is_finite() && cost_usd > 0.0 {
            self.total_dispatch_usd += cost_usd;
            self.cash = (self.cash - cost_usd).max(0.0);
        }
    }

    /// Estimated per-leg dispatch bleed for paper-only mode (live
    /// mode debits actuals instead; never set both). The open leg
    /// debits at open, the close leg pro-rata at settle. Last write
    /// wins (legs move with SOL); zero disables.
    pub fn set_paper_dispatch_legs(&mut self, open_usd: f64, close_usd: f64) {
        self.paper_dispatch_legs = (open_usd, close_usd);
    }

    /// Current legs (open, close). The open leg is what the last open
    /// debited, so the breaker can book it at open while the close
    /// headline only nets the close leg.
    pub fn paper_dispatch_legs(&self) -> (f64, f64) {
        self.paper_dispatch_legs
    }

    /// Cumulative dispatch bleed debited so far.
    pub fn total_dispatch_usd(&self) -> f64 {
        self.total_dispatch_usd
    }

    /// Dispatch-adjusted realized PnL: trade edge minus live bleed.
    /// Use this for headlines/breaker, not [`Self::realized_pnl`].
    pub fn realized_net_pnl(&self) -> f64 {
        self.realized_sum - self.total_dispatch_usd
    }

    pub fn starting_balance(&self) -> f64 {
        self.starting_balance
    }

    pub fn position_ids(&self) -> Vec<PositionId> {
        self.positions.keys().copied().collect()
    }

    pub fn position(&self, id: PositionId) -> Option<&Position> {
        self.positions.get(&id)
    }

    /// Moonbag percent of a position (0 for a gone id). The tick path
    /// and the replay both size the TP scale-out from it.
    pub fn moonbag_of(&self, id: PositionId) -> f64 {
        self.position(id).map(|p| p.moonbag_percent).unwrap_or(0.0)
    }

    pub fn history(&self) -> &[ClosedTrade] {
        &self.history
    }

    pub fn positions_for_mint(&self, mint: &str) -> Vec<PositionId> {
        self.positions
            .iter()
            .filter(|(_, p)| p.mint == mint)
            .map(|(id, _)| *id)
            .collect()
    }

    fn fee_for(&self, notional: f64) -> f64 {
        notional * self.fee_bps / 10_000.0
    }

    /// Open from params.
    pub fn open_position_params(
        &mut self,
        p: &OpenPositionParams<'_>,
    ) -> Result<PositionId, PortfolioError> {
        let notional_usd = p.notional_usd;
        let tp_percent = p.tp_percent;
        let sl_percent = p.sl_percent;
        if !p.mark.price.is_finite() || p.mark.price <= 0.0 {
            return Err(PortfolioError::InvalidPrice(p.mark.price));
        }
        if !notional_usd.is_finite() || notional_usd <= 0.0 {
            return Err(PortfolioError::InvalidNotional(notional_usd));
        }
        if !tp_percent.is_finite() || tp_percent <= 0.0 {
            return Err(PortfolioError::InvalidTakeProfit(tp_percent));
        }
        if !sl_percent.is_finite() || sl_percent >= 0.0 || sl_percent < -100.0 {
            return Err(PortfolioError::InvalidStopLoss(sl_percent));
        }
        if p.mint.is_empty() {
            return Err(PortfolioError::InvalidMint(p.mint.to_string()));
        }
        // Execution pricing: exact curve math when the tick carried
        // reserves, proxy basis otherwise. Reject when the curve cannot
        // absorb the size instead of printing a fictitious capped fill;
        // sub-token dust cannot open (live sizes zero dust to a retry,
        // never to a fill).
        let fill = match self
            .fill_for_mint(p.mint)
            .entry_fill(p.side, &p.mark, notional_usd)
        {
            Ok(fill) => fill,
            Err(EntryFillError::Dust) => {
                return Err(PortfolioError::InvalidNotional(notional_usd));
            }
            Err(_) => {
                return Err(PortfolioError::CurveExhausted {
                    notional: notional_usd,
                    liquidity: p.mark.liquidity_usd,
                });
            }
        };
        let entry_price = fill.price;
        let quantity = fill.quantity;
        // Debit what is actually invested (floored size at the filled
        // price), never the nominal ticket: the old f64 size made them
        // identical, the integer size leaves up to one token unspent.
        let invested = quantity as f64 * entry_price;
        let entry_fee = self.fee_for(invested);

        match p.side {
            Side::Long => {
                let need = invested + entry_fee;
                if need > self.cash {
                    return Err(PortfolioError::InsufficientFunds {
                        need,
                        have: self.cash,
                    });
                }
                self.cash -= need;
            }
            Side::Short => {
                if entry_fee > self.cash {
                    return Err(PortfolioError::InsufficientFunds {
                        need: entry_fee,
                        have: self.cash,
                    });
                }
                self.cash += invested - entry_fee;
            }
        }
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(PortfolioError::IdExhausted)?;
        self.positions.insert(
            id,
            Position {
                id,
                mint: p.mint.to_string(),
                side: p.side,
                entry_price,
                quantity,
                // Actually invested (floored size at fill), not the nominal
                // ticket: basis, percents, and cash all agree on it.
                entry_notional: invested,
                entry_fee,
                tp_percent,
                sl_percent,
                trail_percent: finite_or_zero(p.trail_percent).max(0.0),
                trailing_stop: p.trailing,
                best_price: entry_price,
                opened_at: p.now_secs,
                timeout_seconds: p.timeout_seconds,
                moonbag_percent: finite_or_zero(p.moonbag_percent).clamp(0.0, 100.0),
            },
        );
        // Paper-only bleed (live debits actuals instead).
        self.debit_dispatch_cost(self.paper_dispatch_legs.0);
        Ok(id)
    }

    /// Close a position.
    pub fn close_position(
        &mut self,
        id: PositionId,
        mark: &Mark,
        reason: ExitReason,
        now_secs: u64,
    ) -> Result<ClosedTrade, PortfolioError> {
        self.settle(id, 1.0, mark, reason, now_secs)
    }

    /// Close a fraction.
    pub fn close_partial_position(
        &mut self,
        id: PositionId,
        fraction: f64,
        mark: &Mark,
        reason: ExitReason,
        now_secs: u64,
    ) -> Result<ClosedTrade, PortfolioError> {
        if !fraction.is_finite() || fraction <= 0.0 || fraction > 1.0 {
            return Err(PortfolioError::InvalidFraction(fraction));
        }
        self.settle(id, fraction, mark, reason, now_secs)
    }

    fn settle(
        &mut self,
        id: PositionId,
        fraction: f64,
        mark: &Mark,
        reason: ExitReason,
        now_secs: u64,
    ) -> Result<ClosedTrade, PortfolioError> {
        if !mark.price.is_finite() || mark.price <= 0.0 {
            return Err(PortfolioError::InvalidPrice(mark.price));
        }
        // Execution pricing guard: an unpriced exit (drained curve,
        // unpriceable basis) is caught before mutating. Priced on the
        // same fill the slice will book (floored part quantity).
        {
            let position = self
                .positions
                .get(&id)
                .ok_or(PortfolioError::PositionNotFound(id))?;
            let slice_qty = slice_quantity(position.quantity, fraction);
            let opposed = self.fill_for_mint(&position.mint);
            if slice_qty > 0 && opposed.exit_price(position.side, mark, slice_qty).is_none() {
                return Err(PortfolioError::CurveExhausted {
                    notional: slice_qty as f64 * mark.price,
                    liquidity: mark.liquidity_usd,
                });
            }
        }
        if fraction >= 1.0 {
            let position = self
                .positions
                .remove(&id)
                .ok_or(PortfolioError::PositionNotFound(id))?;
            let trade = self.book(
                &CloseSlice::whole(&position),
                mark,
                reason,
                now_secs,
            )?;
            self.record(trade.clone());
            return Ok(trade);
        }
        let slice = {
            let position = self
                .positions
                .get(&id)
                .ok_or(PortfolioError::PositionNotFound(id))?;
            CloseSlice::part(position, fraction)
        };
        // Partial slices that floor to zero tokens are dust, not fills:
        // reject like live dust (retry budget) instead of booking a
        // zero-size trade with bleed.
        if slice.quantity == 0 {
            return Err(PortfolioError::InvalidFraction(fraction));
        }
        let trade = self.book(&slice, mark, reason, now_secs)?;
        if let Some(open) = self.positions.get_mut(&id) {
            open.quantity -= slice.quantity;
            open.entry_notional -= slice.entry_notional;
            open.entry_fee -= slice.entry_fee;
            // TP scale-out: stop to breakeven, 24h backstop.
            if reason == ExitReason::TakeProfit && fraction < 1.0 {
                open.sl_percent = open.sl_percent.max(0.0);
                open.timeout_seconds = RUNNER_BACKSTOP_SECS;
                open.moonbag_percent = 0.0;
            }
        }
        self.record(trade.clone());
        Ok(trade)
    }

    fn record(&mut self, trade: ClosedTrade) {
        if trade.net_pnl.is_finite() {
            self.realized_sum += trade.net_pnl;
        }
        if trade.net_pnl > 0.0 {
            self.win_count += 1;
        }
        self.closed_total += 1;
        self.history.push(trade);
        if self.history.len() > MAX_HISTORY_TRADES {
            let drop_n = self.history.len() - MAX_HISTORY_TRADES;
            self.history.drain(..drop_n);
        }
    }

    fn book(
        &mut self,
        slice: &CloseSlice,
        mark: &Mark,
        reason: ExitReason,
        now_secs: u64,
    ) -> Result<ClosedTrade, PortfolioError> {
        // Execution pricing, uncapped: the settle probe above priced
        // the identical slice, so this agrees with the trigger by
        // construction and can only fail on an unpriced exit.
        let exit_price = self
            .fill_for_mint(&slice.mint)
            .exit_price(slice.side, mark, slice.quantity)
            .ok_or(PortfolioError::CurveExhausted {
                notional: slice.quantity as f64 * mark.price,
                liquidity: mark.liquidity_usd,
            })?;
        let exit_notional = slice.quantity as f64 * exit_price;
        let exit_fee = self.fee_for(exit_notional);

        // Signed by side (quantity itself is absolute): shorts profit
        // when the exit prints below entry.
        let signed_qty = match slice.side {
            Side::Long => slice.quantity as f64,
            Side::Short => -(slice.quantity as f64),
        };
        let gross_pnl = (exit_price - slice.entry_price) * signed_qty;
        let fees = slice.entry_fee + exit_fee;
        let net_pnl = gross_pnl - fees;
        let net_pnl_percent = if slice.entry_notional > 0.0 {
            net_pnl / slice.entry_notional * 100.0
        } else {
            0.0
        };
        // Paper-only bleed, pro-rata (live debits actuals instead).
        let dispatch =
            finite_or_zero(self.paper_dispatch_legs.1 * slice.fraction).max(0.0);
        self.debit_dispatch_cost(dispatch);

        match slice.side {
            Side::Long => self.cash += exit_notional - exit_fee,
            Side::Short => self.cash -= exit_notional + exit_fee,
        }

        Ok(ClosedTrade {
            id: slice.id,
            mint: slice.mint.clone(),
            side: slice.side,
            entry_price: slice.entry_price,
            exit_price,
            quantity: slice.quantity,
            gross_pnl,
            fees,
            net_pnl,
            net_pnl_percent,
            reason,
            fraction_closed: slice.fraction,
            opened_at: slice.opened_at,
            closed_at: now_secs,
            hold_seconds: now_secs.saturating_sub(slice.opened_at),
            dispatch_usd: dispatch,
        })
    }

    /// Mutable position access.
    pub fn position_mut(&mut self, id: PositionId) -> Option<&mut Position> {
        self.positions.get_mut(&id)
    }

    /// Exit trigger on the realizable fill (fee-aware), not the gross
    /// mark. Evaluated on a full-exit basis; `None` holds (except
    /// timeout) when the fill is unknowable.
    pub fn check_exit_realizable(
        &mut self,
        id: PositionId,
        mark: &Mark,
        now_secs: u64,
    ) -> Option<ExitReason> {
        let mint = self.positions.get(&id)?.mint.clone();
        let fill = self.fill_for_mint(&mint);
        let position = self.positions.get_mut(&id)?;
        crate::market::check_exit_realizable(&fill, position, mark, now_secs)
    }

    /// Net PnL of closed trades.
    pub fn realized_pnl(&self) -> f64 {
        self.realized_sum
    }

    /// Mark-to-market snapshot.
    pub fn snapshot(&self, marks: &HashMap<String, Mark>) -> PortfolioSnapshot {
        let mut locked = 0.0;
        let mut unrealized = 0.0;
        let mut mark_value = 0.0;
        let mut realizable_value = 0.0;
        for position in self.positions.values() {
            let basis = match position.side {
                Side::Long => position.entry_notional,
                Side::Short => -position.entry_notional,
            };
            locked += basis;
            let (mark_v, value) = match marks.get(&position.mint) {
                Some(mark) if mark.price.is_finite() && mark.price > 0.0 => {
                    let mark_v = match position.side {
                        Side::Long => position.quantity as f64 * mark.price,
                        Side::Short => -(position.quantity as f64 * mark.price),
                    };
                    // Same execution pricing as settlement (exact curve
                    // state when the mark carries it), so marks and
                    // books agree. Unpriceable holds the basis instead
                    // of a fiction.
                    match self.fill_for_mint(&position.mint).exit_price(
                        position.side,
                        mark,
                        position.quantity,
                    ) {
                        Some(exit_price) => {
                            let exit_notional = position.quantity as f64 * exit_price;
                            let exit_fee = self.fee_for(exit_notional);
                            let value = match position.side {
                                Side::Long => exit_notional - exit_fee,
                                Side::Short => -(exit_notional + exit_fee),
                            };
                            (mark_v, value)
                        }
                        None => (mark_v, basis),
                    }
                }
                _ => (basis, basis),
            };
            unrealized += value - basis;
            mark_value += mark_v;
            realizable_value += value;
        }
        PortfolioSnapshot {
            cash: self.cash,
            locked,
            equity: self.cash + locked + unrealized,
            realized_pnl: self.realized_pnl(),
            unrealized_pnl: unrealized,
            mark_value,
            realizable_value,
            open_count: self.positions.len(),
            closed_count: self.closed_total,
        }
    }

    /// Win rate, if any closes.
    pub fn win_rate(&self) -> Option<f64> {
        if self.closed_total == 0 {
            return None;
        }
        Some(self.win_count as f64 / self.closed_total as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::{check_exit, move_percent};
    use crate::types::Side;

    fn funded() -> PaperPortfolio {
        PaperPortfolio::new(100.0, 0.0).with_fill_model(FillModel::Exact)
    }

    #[test]
    fn moonbag_scale_out_lifts_timeout_and_disarms() {
        let mut pf = funded();
        let id = pf
            .open_position_params(&OpenPositionParams {
                mint: "MINT",
                side: Side::Long,
                mark: Mark::new(1.0, 1_000_000.0),
                notional_usd: 40.0,
                tp_percent: 60.0,
                sl_percent: -8.0,
                trailing: true,
                trail_percent: 5.0,
                timeout_seconds: 90,
                moonbag_percent: 20.0,
                now_secs: 1000,
            })
            .expect("open");
        let trade = pf
            .close_partial_position(
                id,
                0.8,
                &Mark::new(1.7, 1_000_000.0),
                ExitReason::TakeProfit,
                1001,
            )
            .expect("scale out");
        assert_eq!(trade.fraction_closed, 0.8);
        let runner = pf.position(id).expect("runner");
        assert_eq!(runner.timeout_seconds, RUNNER_BACKSTOP_SECS);
        assert_eq!(runner.moonbag_percent, 0.0);
        assert_eq!(runner.sl_percent, 0.0);
        assert!(runner.trailing_stop);
        let id2 = pf
            .open_position_params(&OpenPositionParams {
                mint: "M2",
                side: Side::Long,
                mark: Mark::new(1.0, 1_000_000.0),
                notional_usd: 10.0,
                tp_percent: 60.0,
                sl_percent: -8.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 90,
                moonbag_percent: 0.0,
                now_secs: 1001,
            })
            .expect("open2");
        pf.close_position(id2, &Mark::new(2.0, 1_000_000.0), ExitReason::TakeProfit, 1002)
            .expect("full close");
        assert!(pf.position(id2).is_none());
    }

    #[test]
    fn open_debits_cash_and_close_credits_pnl() {
        let mut pf = funded();
        let id = pf
            .open_position_params(&OpenPositionParams {
                mint: "MINT",
                side: Side::Long,
                mark: Mark::new(1.0, 1_000_000.0),
                notional_usd: 40.0,
                tp_percent: 75.0,
                sl_percent: -8.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 1000,
            })
            .expect("open");
        assert_eq!(pf.cash(), 60.0);

        let trade = pf
            .close_position(id, &Mark::new(2.0, 1_000_000.0), ExitReason::Manual, 1100)
            .expect("close");
        assert_eq!(trade.gross_pnl, 40.0);
        assert_eq!(trade.net_pnl, 40.0);
        assert_eq!(trade.net_pnl_percent, 100.0);
        assert_eq!(trade.hold_seconds, 100);
        assert_eq!(pf.cash(), 140.0);
        assert_eq!(pf.realized_pnl(), 40.0);
    }

    #[test]
    fn fees_reduce_pnl_on_both_sides() {
        let mut pf = PaperPortfolio::new(100.0, 100.0).with_fill_model(FillModel::Exact);
        let id = pf
            .open_position_params(&OpenPositionParams {
                mint: "MINT",
                side: Side::Long,
                mark: Mark::new(1.0, 1e9),
                notional_usd: 50.0,
                tp_percent: 75.0,
                sl_percent: -8.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 0,
            })
            .expect("open");
        assert!((pf.cash() - 49.50).abs() < 1e-9);
        let trade = pf
            .close_position(id, &Mark::new(1.0, 1e9), ExitReason::Manual, 1)
            .expect("close");
        assert!((trade.fees - 1.0).abs() < 1e-9);
        assert!((trade.net_pnl + 1.0).abs() < 1e-9);
        assert!((pf.cash() - 99.0).abs() < 1e-9);
    }

    #[test]
    fn short_roundtrip_flat_price_nets_minus_fees() {
        let mut pf = PaperPortfolio::new(100.0, 100.0).with_fill_model(FillModel::Exact);
        let id = pf
            .open_position_params(&OpenPositionParams {
                mint: "MINT",
                side: Side::Short,
                mark: Mark::new(1.0, 1e9),
                notional_usd: 50.0,
                tp_percent: 75.0,
                sl_percent: -8.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 0,
            })
            .expect("open");
        assert!((pf.cash() - 149.50).abs() < 1e-9, "cash={}", pf.cash());
        assert_eq!(pf.position(id).expect("p").quantity, 50);
        let trade = pf
            .close_position(id, &Mark::new(1.0, 1e9), ExitReason::Manual, 1)
            .expect("close");
        assert!((trade.gross_pnl).abs() < 1e-9, "gross={}", trade.gross_pnl);
        assert!((trade.fees - 1.0).abs() < 1e-9, "fees={}", trade.fees);
        assert!((trade.net_pnl + 1.0).abs() < 1e-9, "net={}", trade.net_pnl);
        assert!((pf.cash() - 99.0).abs() < 1e-9, "cash={}", pf.cash());
        let mut pf2 = PaperPortfolio::new(100.0, 100.0).with_fill_model(FillModel::Exact);
        let id2 = pf2
            .open_position_params(&OpenPositionParams {
                mint: "MINT",
                side: Side::Short,
                mark: Mark::new(1.0, 1e9),
                notional_usd: 50.0,
                tp_percent: 75.0,
                sl_percent: -8.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 0,
            })
            .expect("open");
        let mut marks = HashMap::new();
        marks.insert("MINT".to_string(), Mark::price_only(1.0));
        let snap = pf2.snapshot(&marks);
        let trade2 = pf2
            .close_position(id2, &Mark::new(1.0, 1e9), ExitReason::Manual, 1)
            .expect("close");
        assert!((pf2.cash() - snap.equity).abs() < 1e-9);
        assert!((trade2.net_pnl + 1.0).abs() < 1e-9);
    }

    #[test]
    fn rejects_oversize_and_bad_inputs() {
        let mut pf = funded();
        assert!(matches!(
            pf.open_position_params(&OpenPositionParams {
                mint: "M",
                side: Side::Long,
                mark: Mark::new(1.0, 1e9),
                notional_usd: 200.0,
                tp_percent: 1.0,
                sl_percent: -1.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 0,
            }),
            Err(PortfolioError::InsufficientFunds { .. })
        ));
        assert!(matches!(
            pf.open_position_params(&OpenPositionParams {
                mint: "M",
                side: Side::Long,
                mark: Mark::new(0.0, 1e9),
                notional_usd: 10.0,
                tp_percent: 1.0,
                sl_percent: -1.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 0,
            }),
            Err(PortfolioError::InvalidPrice(_))
        ));
        assert!(matches!(
            pf.open_position_params(&OpenPositionParams {
                mint: "M",
                side: Side::Long,
                mark: Mark::new(1.0, 1e9),
                notional_usd: -5.0,
                tp_percent: 1.0,
                sl_percent: -1.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 0,
            }),
            Err(PortfolioError::InvalidNotional(_))
        ));
        assert!(matches!(
            pf.close_position(999, &Mark::new(1.0, 1e9), ExitReason::Manual, 0),
            Err(PortfolioError::PositionNotFound(999))
        ));
    }

    #[test]
    fn slippage_shrinks_size() {
        let mut pf = PaperPortfolio::new(100.0, 0.0);
        let id = pf
            .open_position_params(&OpenPositionParams {
                mint: "MINT",
                side: Side::Long,
                mark: Mark::new(1.0, 5_000.0),
                notional_usd: 50.0,
                tp_percent: 75.0,
                sl_percent: -8.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 0,
            })
            .expect("open");
        let position = pf.position(id).expect("position");
        assert!(position.entry_price > 1.0);
        assert!(position.quantity < 50);
    }

    #[test]
    fn snapshot_marks_unrealized() {
        let mut pf = funded();
        pf.open_position_params(&OpenPositionParams {
            mint: "A",
            side: Side::Long,
            mark: Mark::new(1.0, 1e9),
            notional_usd: 40.0,
            tp_percent: 75.0,
            sl_percent: -8.0,
            trailing: false,
            trail_percent: 0.0,
            timeout_seconds: 0,
            moonbag_percent: 0.0,
            now_secs: 0,
        })
        .expect("open");
        let mut marks = HashMap::new();
        marks.insert(
            "A".to_string(),
            Mark {
                price: 1.5,
                liquidity_usd: 1e9,
                curve: None,
            },
        );
        let snap = pf.snapshot(&marks);
        assert_eq!(snap.cash, 60.0);
        assert_eq!(snap.locked, 40.0);
        assert_eq!(snap.unrealized_pnl, 20.0);
        assert_eq!(snap.equity, 120.0);
        assert_eq!(snap.open_count, 1);
    }

    #[test]
    fn snapshot_matches_realizable_close() {
        let mut pf = PaperPortfolio::new(100.0, 100.0);
        let id = pf
            .open_position_params(&OpenPositionParams {
                mint: "A",
                side: Side::Long,
                mark: Mark::new(1.0, 5_000.0),
                notional_usd: 40.0,
                tp_percent: 75.0,
                sl_percent: -8.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 0,
            })
            .expect("open");
        let mut marks = HashMap::new();
        marks.insert(
            "A".to_string(),
            Mark {
                price: 1.5,
                liquidity_usd: 5_000.0,
                curve: None,
            },
        );
        let snap = pf.snapshot(&marks);
        let naive = (1.5 - pf.position(id).expect("p").entry_price)
            * pf.position(id).expect("p").quantity as f64;
        assert!(snap.unrealized_pnl < naive);
        // Entry fee on the actually-invested size (100bps commission on
        // `entry_notional`, the size this portfolio was built with), not
        // the nominal ticket: integer sizing leaves up to one token unspent.
        // Read before the close removes the position.
        let entry_fee =
            pf.position(id).expect("p").entry_notional * 100.0 / 10_000.0;
        let trade = pf
            .close_position(id, &Mark::new(1.5, 5_000.0), ExitReason::Manual, 1)
            .expect("close");
        assert!((trade.net_pnl - (snap.unrealized_pnl - entry_fee)).abs() < 1e-9);
        assert!((pf.cash() - snap.equity).abs() < 1e-9);
        let mut pf2 = PaperPortfolio::new(100.0, 0.0);
        pf2.open_position_params(&OpenPositionParams {
            mint: "B",
            side: Side::Long,
            mark: Mark::new(1.0, 1e9),
            notional_usd: 10.0,
            tp_percent: 75.0,
            sl_percent: -8.0,
            trailing: false,
            trail_percent: 0.0,
            timeout_seconds: 0,
            moonbag_percent: 0.0,
            now_secs: 0,
        })
        .expect("open");
        let empty: HashMap<String, Mark> = HashMap::new();
        assert_eq!(pf2.snapshot(&empty).unrealized_pnl, 0.0);
        let mut nan_marks = HashMap::new();
        nan_marks.insert(
            "B".to_string(),
            Mark {
                price: f64::NAN,
                liquidity_usd: 5_000.0,
                curve: None,
            },
        );
        assert_eq!(pf2.snapshot(&nan_marks).unrealized_pnl, 0.0);
    }

    #[test]
    fn full_poll_loop_tp_exit() {
        let mut pf = funded();
        let id = pf
            .open_position_params(&OpenPositionParams {
                mint: "MINT",
                side: Side::Long,
                mark: Mark::new(1.0, 1e9),
                notional_usd: 50.0,
                tp_percent: 75.0,
                sl_percent: -8.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 0,
            })
            .expect("open");
        let reason = {
            let position = pf.position_mut(id).expect("position");
            check_exit(position, 1.80, 10)
        };
        assert_eq!(reason, Some(ExitReason::TakeProfit));
        let trade = pf
            .close_position(
                id,
                &Mark::new(1.80, 1e9),
                reason.unwrap_or(ExitReason::Manual),
                10,
            )
            .expect("close");
        assert_eq!(trade.reason, ExitReason::TakeProfit);
        assert!((trade.net_pnl - 40.0).abs() < 1e-9);
        assert_eq!(pf.win_rate(), Some(1.0));
    }

    #[test]
    fn losing_streak_math() {
        let mut pf = funded();
        for i in 0..2 {
            let id = pf
                .open_position_params(&OpenPositionParams {
                    mint: "M",
                    side: Side::Long,
                    mark: Mark::new(1.0, 1e9),
                    notional_usd: 10.0,
                    tp_percent: 75.0,
                    sl_percent: -8.0,
                    trailing: false,
                    trail_percent: 0.0,
                    timeout_seconds: 0,
                    moonbag_percent: 0.0,
                    now_secs: i,
                })
                .expect("open");
            pf.close_position(id, &Mark::new(0.9, 1e9), ExitReason::StopLoss, i + 1)
                .expect("close");
        }
        assert!((pf.realized_pnl() + 2.0).abs() < 1e-9);
        assert_eq!(pf.win_rate(), Some(0.0));
        assert_eq!(pf.history().len(), 2);
    }

    #[test]
    fn rejects_degenerate_tp_sl() {
        let mut pf = funded();
        for (tp, sl) in [(0.0, -8.0), (-5.0, -8.0), (75.0, 0.0), (75.0, 5.0)] {
            assert!(
                matches!(
                    pf.open_position_params(&OpenPositionParams {
                        mint: "M",
                        side: Side::Long,
                        mark: Mark::new(1.0, 1e9),
                        notional_usd: 10.0,
                        tp_percent: tp,
                        sl_percent: sl,
                        trailing: false,
                        trail_percent: 0.0,
                        timeout_seconds: 0,
                        moonbag_percent: 0.0,
                        now_secs: 0,
                    }),
                    Err(PortfolioError::InvalidTakeProfit(_))
                        | Err(PortfolioError::InvalidStopLoss(_))
                ),
                "tp={tp} sl={sl}"
            );
        }
        assert_eq!(pf.cash(), 100.0);
    }

    #[test]
    fn move_percent_degenerate_is_zero() {
        assert_eq!(move_percent(Side::Long, 0.0, 1.0), 0.0);
    }

    fn open_40_at_1(pf: &mut PaperPortfolio) -> PositionId {
        pf.open_position_params(&OpenPositionParams {
            mint: "MINT",
            side: Side::Long,
            mark: Mark::new(1.0, 1e9),
            notional_usd: 40.0,
            tp_percent: 75.0,
            sl_percent: -8.0,
            trailing: false,
            trail_percent: 0.0,
            timeout_seconds: 0,
            moonbag_percent: 0.0,
            now_secs: 1000,
        })
        .expect("open")
    }

    #[test]
    fn partial_80_20_split_matches_full_close_totals() {
        let mut pf = funded();
        let id = open_40_at_1(&mut pf);
        let part = pf
            .close_partial_position(id, 0.8, &Mark::new(2.0, 1e9), ExitReason::TakeProfit, 1100)
            .expect("partial");
        assert_eq!(part.fraction_closed, 0.8);
        assert_eq!(part.quantity, 32);
        assert_eq!(part.gross_pnl, 32.0);
        assert_eq!(part.net_pnl, 32.0);
        assert_eq!(part.net_pnl_percent, 100.0);
        assert_eq!(pf.cash(), 60.0 + 32.0 + 32.0);

        let open = pf.position(id).expect("runner");
        assert_eq!((open.quantity, open.entry_notional), (8, 8.0));
        assert_eq!((open.tp_percent, open.sl_percent), (75.0, 0.0));

        let rest = pf
            .close_partial_position(id, 1.0, &Mark::new(2.0, 1e9), ExitReason::TakeProfit, 1200)
            .expect("rest");
        assert_eq!(rest.fraction_closed, 1.0);
        assert!(pf.position(id).is_none());
        assert_eq!(pf.cash(), 140.0);
        assert_eq!(pf.realized_pnl(), 40.0);
        assert_eq!(pf.history().len(), 2);
    }

    #[test]
    fn tp_scale_out_ratchets_runner_to_breakeven() {
        let mut pf = funded();
        let id = open_40_at_1(&mut pf);
        assert_eq!(pf.position(id).expect("p").sl_percent, -8.0);
        pf.close_partial_position(id, 0.8, &Mark::new(2.0, 1e9), ExitReason::TakeProfit, 1)
            .expect("partial");
        assert_eq!(pf.position(id).expect("runner").sl_percent, 0.0);
    }

    #[test]
    fn ratchet_never_loosens_and_ignores_other_reasons() {
        let mut pf = funded();
        let id = open_40_at_1(&mut pf);
        pf.position_mut(id).expect("p").sl_percent = 5.0;
        pf.close_partial_position(id, 0.5, &Mark::new(2.0, 1e9), ExitReason::TakeProfit, 1)
            .expect("partial");
        assert_eq!(pf.position(id).expect("p").sl_percent, 5.0);

        let id2 = open_40_at_1(&mut pf);
        pf.close_partial_position(id2, 0.5, &Mark::new(2.0, 1e9), ExitReason::Manual, 1)
            .expect("partial");
        assert_eq!(pf.position(id2).expect("p").sl_percent, -8.0);
    }

    #[test]
    fn manual_partial_preserves_runner_terms() {
        let mut pf = funded();
        let id = pf
            .open_position_params(&OpenPositionParams {
                mint: "M",
                side: Side::Long,
                mark: Mark::new(1.0, 1e9),
                notional_usd: 10.0,
                tp_percent: 75.0,
                sl_percent: -8.0,
                trailing: true,
                trail_percent: 5.0,
                timeout_seconds: 90,
                moonbag_percent: 20.0,
                now_secs: 0,
            })
            .expect("open");
        pf.close_partial_position(id, 0.5, &Mark::new(1.5, 1e9), ExitReason::Manual, 1)
            .expect("partial");
        let p = pf.position(id).expect("runner");
        assert_eq!(p.sl_percent, -8.0);
        assert_eq!(p.timeout_seconds, 90);
        assert_eq!(p.moonbag_percent, 20.0);
        pf.close_partial_position(id, 0.5, &Mark::new(2.0, 1e9), ExitReason::TakeProfit, 2)
            .expect("partial");
        let p = pf.position(id).expect("runner");
        assert_eq!(p.sl_percent, 0.0);
        assert_eq!(p.timeout_seconds, RUNNER_BACKSTOP_SECS);
        assert_eq!(p.moonbag_percent, 0.0);
    }

    #[test]
    fn rejects_empty_mint() {
        let mut pf = funded();
        assert!(matches!(
            pf.open_position_params(&OpenPositionParams {
                mint: "",
                side: Side::Long,
                mark: Mark::new(1.0, 1e9),
                notional_usd: 10.0,
                tp_percent: 75.0,
                sl_percent: -8.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 0,
            }),
            Err(PortfolioError::InvalidMint(_))
        ));
    }

    #[test]
    fn nan_trail_and_moonbag_normalize_to_zero() {
        let mut pf = funded();
        let id = pf
            .open_position_params(&OpenPositionParams {
                mint: "M",
                side: Side::Long,
                mark: Mark::new(1.0, 1e9),
                notional_usd: 10.0,
                tp_percent: 75.0,
                sl_percent: -8.0,
                trailing: true,
                trail_percent: f64::NAN,
                timeout_seconds: 90,
                moonbag_percent: f64::NAN,
                now_secs: 0,
            })
            .expect("open");
        let p = pf.position(id).expect("p");
        assert_eq!(p.trail_percent, 0.0);
        assert_eq!(p.moonbag_percent, 0.0);
    }

    #[test]
    fn custom_venue_fee_prices_worse_fills_per_mint() {
        let mut pf = PaperPortfolio::new(1_000.0, 0.0);
        pf.set_venue_fee_for_mint("CUSTOM", 345.0);
        assert!((pf.venue_fee_for("CUSTOM") - 345.0).abs() < 1e-9);
        assert!((pf.venue_fee_for("STD") - 125.0).abs() < 1e-9);
        let std_id = pf
            .open_position_params(&OpenPositionParams {
                mint: "STD",
                side: Side::Long,
                mark: Mark::new(1.0, 1e9),
                notional_usd: 100.0,
                tp_percent: 75.0,
                sl_percent: -8.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 0,
            })
            .expect("std open");
        let cus_id = pf
            .open_position_params(&OpenPositionParams {
                mint: "CUSTOM",
                side: Side::Long,
                mark: Mark::new(1.0, 1e9),
                notional_usd: 100.0,
                tp_percent: 75.0,
                sl_percent: -8.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 0,
            })
            .expect("custom open");
        let std_px = pf.position(std_id).expect("std").entry_price;
        let cus_px = pf.position(cus_id).expect("cus").entry_price;
        assert!(cus_px > std_px, "custom {cus_px} vs std {std_px}");
        assert!((cus_px - std_px - 0.022).abs() < 1e-9, "cus={cus_px} std={std_px}");
        pf.clear_venue_fee_for_mint("CUSTOM");
        assert!((pf.venue_fee_for("CUSTOM") - 125.0).abs() < 1e-9);
        pf.set_venue_fee_for_mint("", 999.0);
        pf.set_venue_fee_for_mint("BAD", f64::NAN);
        pf.set_venue_fee_for_mint("NEG", -5.0);
        assert!((pf.venue_fee_for("BAD") - 125.0).abs() < 1e-9);
        assert!((pf.venue_fee_for("NEG") - 125.0).abs() < 1e-9);
    }

    #[test]
    fn snapshot_ignores_degenerate_marks() {
        let mut pf = funded();
        open_40_at_1(&mut pf);
        let base = pf.snapshot(&HashMap::new());
        for price in [0.0, -2.0, f64::NAN] {
            let mut marks = HashMap::new();
            marks.insert("MINT".to_string(), Mark::new(price, 1e9));
            let snap = pf.snapshot(&marks);
            assert_eq!(snap.unrealized_pnl, base.unrealized_pnl, "price {price}");
        }
    }

    #[test]
    fn partial_short_splits_absolute_quantity() {
        let mut pf = funded();
        let id = pf
            .open_position_params(&OpenPositionParams {
                mint: "MINT",
                side: Side::Short,
                mark: Mark::new(2.0, 1e9),
                notional_usd: 40.0,
                tp_percent: 75.0,
                sl_percent: -8.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 0,
            })
            .expect("open");
        assert_eq!(pf.position(id).expect("p").quantity, 20);
        let part = pf
            .close_partial_position(id, 0.5, &Mark::new(1.0, 1e9), ExitReason::TakeProfit, 1)
            .expect("partial");
        assert_eq!(part.quantity, 10);
        assert_eq!(part.gross_pnl, 10.0);
        assert_eq!(pf.position(id).expect("p").quantity, 10);
    }

    #[test]
    fn integer_sizing_rejects_dust() {
        // Sub-token opens cannot exist (live sizes zero dust to a retry,
        // never to a fill): fail closed instead of booking a zero-size
        // position that still bleeds dispatch.
        let mut pf = funded();
        assert!(matches!(
            pf.open_position_params(&OpenPositionParams {
                mint: "MINT",
                side: Side::Long,
                mark: Mark::new(100.0, 1e9),
                notional_usd: 10.0,
                tp_percent: 75.0,
                sl_percent: -8.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 0,
            }),
            Err(PortfolioError::InvalidNotional(_))
        ));
        // A partial that floors to zero tokens is dust, not a fill.
        let id = pf
            .open_position_params(&OpenPositionParams {
                mint: "MINT",
                side: Side::Long,
                mark: Mark::new(1.0, 1e9),
                notional_usd: 1.0,
                tp_percent: 75.0,
                sl_percent: -8.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 0,
            })
            .expect("one token opens");
        assert_eq!(pf.position(id).expect("p").quantity, 1);
        assert!(matches!(
            pf.close_partial_position(id, 0.5, &Mark::new(1.0, 1e9), ExitReason::Manual, 1),
            Err(PortfolioError::InvalidFraction(_))
        ));
        // Whole-token partials still settle exactly (no float remainder).
        let id2 = pf
            .open_position_params(&OpenPositionParams {
                mint: "M2",
                side: Side::Long,
                mark: Mark::new(1.0, 1e9),
                notional_usd: 10.0,
                tp_percent: 75.0,
                sl_percent: -8.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 0,
            })
            .expect("ten tokens open");
        let part = pf
            .close_partial_position(id2, 0.3, &Mark::new(1.0, 1e9), ExitReason::Manual, 1)
            .expect("partial");
        assert_eq!(part.quantity, 3);
        assert_eq!(pf.position(id2).expect("p").quantity, 7);
    }

    #[test]
    fn slice_quantity_floors_never_rounds() {
        // Paper book and live sell mirror both size through this, so a
        // scale-out can never drift between them by a token.
        assert_eq!(slice_quantity(10_000, 1.0), 10_000);
        assert_eq!(slice_quantity(10_000, 1.5), 10_000);
        assert_eq!(slice_quantity(10_000, f64::INFINITY), 10_000);
        assert_eq!(slice_quantity(10_000, 0.3), 3_000);
        assert_eq!(slice_quantity(9_999, 0.00015), 1);
        assert_eq!(slice_quantity(100, 0.999), 99);
        assert_eq!(
            slice_quantity(1_000_000_000_000_000, 0.5),
            500_000_000_000_000
        );
        // Dust and garbage slice to zero (book rejects as dust; live
        // counts it toward the retry budget).
        assert_eq!(slice_quantity(9_999, 0.0001), 0);
        assert_eq!(slice_quantity(100, 0.0), 0);
        assert_eq!(slice_quantity(100, -0.5), 0);
        assert_eq!(slice_quantity(100, f64::NAN), 0);
    }

    #[test]
    fn partial_rejects_bad_fractions_and_missing_ids() {
        let mut pf = funded();
        let id = open_40_at_1(&mut pf);
        for bad in [0.0, -0.5, 1.5, f64::NAN, f64::INFINITY] {
            assert!(
                matches!(
                    pf.close_partial_position(id, bad, &Mark::new(2.0, 1e9), ExitReason::Manual, 0),
                    Err(PortfolioError::InvalidFraction(_))
                ),
                "fraction={bad}"
            );
        }
        assert!(matches!(
            pf.close_partial_position(999, 0.5, &Mark::new(2.0, 1e9), ExitReason::Manual, 0),
            Err(PortfolioError::PositionNotFound(999))
        ));
        assert_eq!(pf.position(id).expect("p").quantity, 40);
        assert_eq!(pf.cash(), 60.0);
    }

    #[test]
    fn paper_dispatch_legs_bleed_cash_and_tag_trades() {
        let mut pf = funded();
        pf.set_paper_dispatch_legs(0.30, 0.05);
        let id = open_40_at_1(&mut pf);
        // Open leg bled from cash.
        assert!((pf.cash() - (60.0 - 0.30)).abs() < 1e-9, "cash={}", pf.cash());
        let trade = pf
            .close_position(id, &Mark::new(2.0, 1e9), ExitReason::Manual, 1100)
            .expect("close");
        // Close leg pro-rata on the trade; edge untouched.
        assert!((trade.dispatch_usd - 0.05).abs() < 1e-9);
        assert_eq!(trade.net_pnl, 40.0);
        assert!((pf.cash() - (140.0 - 0.35)).abs() < 1e-9, "cash={}", pf.cash());
        assert!((pf.total_dispatch_usd() - 0.35).abs() < 1e-9);
        assert!((pf.realized_net_pnl() - (40.0 - 0.35)).abs() < 1e-9);
    }

    #[test]
    fn paper_dispatch_partial_close_pro_rata() {
        let mut pf = funded();
        pf.set_paper_dispatch_legs(0.0, 0.10);
        let id = open_40_at_1(&mut pf);
        let part = pf
            .close_partial_position(id, 0.5, &Mark::new(2.0, 1e9), ExitReason::Manual, 1100)
            .expect("partial");
        assert!((part.dispatch_usd - 0.05).abs() < 1e-9);
    }

    #[test]
    fn paper_dispatch_defaults_off() {
        // Zero legs: legacy accounting bit-for-bit.
        let mut pf = funded();
        let id = open_40_at_1(&mut pf);
        assert_eq!(pf.cash(), 60.0);
        let trade = pf
            .close_position(id, &Mark::new(2.0, 1e9), ExitReason::Manual, 1100)
            .expect("close");
        assert_eq!(trade.dispatch_usd, 0.0);
        assert_eq!(pf.cash(), 140.0);
    }

    #[test]
    fn debit_dispatch_cost_bleeds_cash_and_floors_at_zero() {
        let mut pf = PaperPortfolio::new(10.0, 0.0);
        pf.debit_dispatch_cost(2.5);
        assert!((pf.cash() - 7.5).abs() < 1e-9);
        assert!((pf.total_dispatch_usd() - 2.5).abs() < 1e-9);
        assert!((pf.realized_net_pnl() + 2.5).abs() < 1e-9);
        pf.debit_dispatch_cost(100.0);
        assert_eq!(pf.cash(), 0.0);
        assert!((pf.total_dispatch_usd() - 102.5).abs() < 1e-9);
        let mut pf2 = PaperPortfolio::new(10.0, 0.0);
        for bad in [f64::NAN, f64::INFINITY, -1.0, 0.0] {
            pf2.debit_dispatch_cost(bad);
            assert_eq!(pf2.cash(), 10.0, "bad={bad}");
        }
        assert_eq!(pf2.total_dispatch_usd(), 0.0);
    }

    #[test]
    fn dispatch_net_turns_thin_win_into_loss() {
        let mut pf = PaperPortfolio::new(100.0, 0.0).with_fill_model(FillModel::Exact);
        let id = pf
            .open_position_params(&OpenPositionParams {
                mint: "MINT",
                side: Side::Long,
                mark: Mark::new(1.0, 1e9),
                notional_usd: 10.0,
                tp_percent: 100.0,
                sl_percent: -50.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 0,
            })
            .expect("open");
        let trade = pf
            .close_position(id, &Mark::new(1.02, 1e9), ExitReason::Manual, 1)
            .expect("close");
        assert!(trade.net_pnl > 0.0);
        pf.debit_dispatch_cost(0.34);
        assert!(pf.realized_net_pnl() < trade.net_pnl);
        assert!((pf.realized_net_pnl() - (trade.net_pnl - 0.34)).abs() < 1e-9);
    }

    #[test]
    fn rejects_curve_exhaustion_and_absurd_stops() {
        let mut pf = PaperPortfolio::new(1_000_000.0, 0.0);
        assert!(matches!(
            pf.open_position_params(&OpenPositionParams {
                mint: "MINT",
                side: Side::Long,
                mark: Mark::new(1.0, 10_000.0),
                notional_usd: 10_000_000.0,
                tp_percent: 100.0,
                sl_percent: -50.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 0,
            }),
            Err(PortfolioError::CurveExhausted { .. })
        ));
        for bad_sl in [-204.0, -101.0, 0.0, 5.0, f64::NAN] {
            assert!(
                matches!(
                    pf.open_position_params(&OpenPositionParams {
                        mint: "MINT",
                        side: Side::Long,
                        mark: Mark::new(1.0, 1e9),
                        notional_usd: 10.0,
                        tp_percent: 100.0,
                        sl_percent: bad_sl,
                        trailing: false,
                        trail_percent: 0.0,
                        timeout_seconds: 0,
                        moonbag_percent: 0.0,
                        now_secs: 0,
                    }),
                    Err(PortfolioError::InvalidStopLoss(_))
                ),
                "sl={bad_sl}"
            );
        }
    }

    #[test]
    fn giant_long_exit_books_true_impact_not_capped() {
        // Liquidity evaporates between entry and exit: the exit sizes
        // ~10x the remaining book. Old code printed 0.75 * quote
        // (capped); honest pricing prints true impact (~0.08 here).
        let mut pf = PaperPortfolio::new(10_000.0, 0.0);
        let id = pf
            .open_position_params(&OpenPositionParams {
                mint: "MINT",
                side: Side::Long,
                mark: Mark::new(1.0, 1e9),
                notional_usd: 100.0,
                tp_percent: 100.0,
                sl_percent: -50.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 0,
            })
            .expect("open");
        // Trigger and settlement must agree (no phantom trigger on a
        // capped fiction): the realizable SL fires here because true
        // proceeds are ~-92%, not -25%.
        assert_eq!(
            pf.check_exit_realizable(id, &Mark::new(1.0, 10.0), 1),
            Some(ExitReason::StopLoss)
        );
        let trade = pf
            .close_position(id, &Mark::new(1.0, 10.0), ExitReason::StopLoss, 1)
            .expect("close");
        assert!(
            trade.exit_price < 0.5,
            "exit must print true impact, not 0.75: {}",
            trade.exit_price
        );
        assert!(trade.net_pnl < -50.0, "net={}", trade.net_pnl);
    }

    #[test]
    fn short_exit_in_cap_band_prices_true_impact() {
        // Short exit at s = 0.5: true slip is 1.0125, not the 0.25 cap.
        let mut pf = PaperPortfolio::new(100_000.0, 0.0);
        let id = pf
            .open_position_params(&OpenPositionParams {
                mint: "MINT",
                side: Side::Short,
                mark: Mark::new(1.0, 1e9),
                notional_usd: 100.0,
                tp_percent: 100.0,
                sl_percent: -50.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 0,
            })
            .expect("open");
        let qty = pf.positions.get(&id).expect("open").quantity as f64;
        let trade = pf
            .close_position(id, &Mark::new(1.0, 200.0), ExitReason::Manual, 1)
            .expect("close");
        // Exit notional drifts off round 100 by the entry fee; price
        // the true size, not a nominal one.
        let s = qty * 1.0 / 200.0;
        let want = 1.0 * (1.0 + s / (1.0 - s) + 0.0125);
        assert!(
            (trade.exit_price - want).abs() < 1e-9,
            "exit={} want={} s={s}",
            trade.exit_price,
            want
        );
        assert!(
            trade.exit_price > 1.25,
            "must print through the 0.25 cap: {}",
            trade.exit_price
        );
    }

    #[test]
    fn history_caps_detail_but_not_accounting() {
        let mut pf = PaperPortfolio::new(1_000_000.0, 0.0);
        let n = MAX_HISTORY_TRADES + 500;
        for i in 0..n {
            let id = pf
                .open_position_params(&OpenPositionParams {
                    mint: "MINT",
                    side: Side::Long,
                    mark: Mark::new(1.0, 1e9),
                    notional_usd: 10.0,
                    tp_percent: 100.0,
                    sl_percent: -50.0,
                    trailing: false,
                    trail_percent: 0.0,
                    timeout_seconds: 0,
                    moonbag_percent: 0.0,
                    now_secs: i as u64,
                })
                .expect("open");
            pf.close_position(id, &Mark::new(1.0, 1e9), ExitReason::Manual, i as u64)
                .expect("close");
        }
        let _ = HashMap::<String, ()>::new();
        assert_eq!(pf.history().len(), MAX_HISTORY_TRADES);
        let snap = pf.snapshot(&HashMap::new());
        assert!(snap.realized_pnl.is_finite());
        assert_eq!(snap.closed_count, n);
        assert_eq!(pf.win_rate(), Some(0.0));
    }

    /// Pump.fun launch scale: 30 SOL virtual / 1.073e15 virtual tokens /
    /// 793.1e12 real tokens, 6 decimals, SOL $200.
    fn launch_curve() -> crate::market::CurveReserves {
        crate::market::CurveReserves {
            vq_lamports: 30_000_000_000,
            vt_base: 1_073_000_000_000_000,
            real_tokens: 793_100_000_000_000,
            decimals: 6,
            sol_usd: 200.0,
        }
    }

    #[test]
    fn exact_curve_open_and_close_are_state_based() {
        let mut pf = PaperPortfolio::new(100.0, 0.0);
        let spot = 5.591_798_695_246_971e-6;
        let mark = Mark::new(spot, 6_000.0).with_curve(launch_curve());
        let id = pf
            .open_position_params(&OpenPositionParams {
                mint: "MINT",
                side: Side::Long,
                mark,
                notional_usd: 10.0,
                tp_percent: 100.0,
                sl_percent: -50.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 1000,
            })
            .expect("exact open");
        let position = pf.position(id).expect("open").clone();
        // Exact curve size, not a proxy estimate; fee inside the price.
        assert_eq!(position.quantity, 1_763_352);
        assert!(
            position.entry_price > spot,
            "entry={}",
            position.entry_price
        );
        assert!(position.entry_notional <= 10.0);

        // Marks price the impact-free spot; equity books the realizable
        // exit (size impact + venue fee) — the discount is explicit.
        let marks = HashMap::from([("MINT".to_string(), mark)]);
        let snap = pf.snapshot(&marks);
        assert!(snap.realizable_value < snap.mark_value);
        assert!((snap.unrealized_pnl - (snap.realizable_value - snap.locked)).abs() < 1e-9);
        assert!(
            (snap.exit_discount_usd() - (snap.mark_value - snap.realizable_value)).abs() < 1e-12
        );
        assert!(snap.exit_discount_usd() > 0.0);

        // The close books the same exact sell the snapshot priced.
        let expected_exit = launch_curve()
            .sell_fill_price(1_763_352, 125.0)
            .expect("priced");
        let trade = pf
            .close_position(id, &mark, ExitReason::Manual, 1100)
            .expect("close");
        assert!((trade.exit_price - expected_exit).abs() < 1e-15);
        assert!(trade.exit_price < spot);
    }
}

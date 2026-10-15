//! Virtual portfolio.

use crate::market::FillModel;
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
    quantity: f64,
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
            quantity: position.quantity * fraction,
            entry_notional: position.entry_notional * fraction,
            entry_fee: position.entry_fee * fraction,
            opened_at: position.opened_at,
            fraction,
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

/// Finite value or zero.
fn finite_or_zero(v: f64) -> f64 {
    if v.is_finite() { v } else { 0.0 }
}

/// Mark-to-market quote for one mint.
#[derive(Debug, Clone, Copy)]
pub struct Mark {
    pub price: f64,
    pub liquidity_usd: f64,
}

impl Mark {
    /// Price without liquidity.
    pub fn price_only(price: f64) -> Self {
        Self { price, liquidity_usd: f64::NAN }
    }
}

/// Open-position inputs.
#[derive(Debug, Clone)]
pub struct OpenPositionParams<'a> {
    pub mint: &'a str,
    pub side: Side,
    pub quote: f64,
    pub notional_usd: f64,
    pub liquidity_usd: f64,
    pub tp_percent: f64,
    pub sl_percent: f64,
    pub trailing: bool,
    pub trail_percent: f64,
    pub timeout_seconds: u64,
    pub moonbag_percent: f64,
    pub now_secs: u64,
}

/// Accounting snapshot.
#[derive(Debug, Clone)]
pub struct PortfolioSnapshot {
    pub cash: f64,
    pub locked: f64,
    pub equity: f64,
    pub realized_pnl: f64,
    pub unrealized_pnl: f64,
    pub open_count: usize,
    pub closed_count: usize,
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

    /// Open a position.
    #[allow(clippy::too_many_arguments)]
    pub fn open_position(
        &mut self,
        mint: &str,
        side: Side,
        quote: f64,
        notional_usd: f64,
        liquidity_usd: f64,
        tp_percent: f64,
        sl_percent: f64,
        trailing: bool,
        trail_percent: f64,
        timeout_seconds: u64,
        moonbag_percent: f64,
        now_secs: u64,
    ) -> Result<PositionId, PortfolioError> {
        self.open_position_params(&OpenPositionParams {
            mint,
            side,
            quote,
            notional_usd,
            liquidity_usd,
            tp_percent,
            sl_percent,
            trailing,
            trail_percent,
            timeout_seconds,
            moonbag_percent,
            now_secs,
        })
    }

    /// Open from params.
    pub fn open_position_params(
        &mut self,
        p: &OpenPositionParams<'_>,
    ) -> Result<PositionId, PortfolioError> {
        let quote = p.quote;
        let notional_usd = p.notional_usd;
        let tp_percent = p.tp_percent;
        let sl_percent = p.sl_percent;
        if !quote.is_finite() || quote <= 0.0 {
            return Err(PortfolioError::InvalidPrice(quote));
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
        // Execution pricing: reject when the curve cannot absorb the
        // size instead of printing a fictitious capped fill.
        let entry_price = self
            .fill_for_mint(p.mint)
            .try_fill_price(p.side, quote, notional_usd, p.liquidity_usd)
            .ok_or(PortfolioError::CurveExhausted {
                notional: notional_usd,
                liquidity: p.liquidity_usd,
            })?;
        let entry_fee = self.fee_for(notional_usd);

        let quantity = match p.side {
            Side::Long => notional_usd / entry_price,
            Side::Short => -(notional_usd / entry_price),
        };

        match p.side {
            Side::Long => {
                let need = notional_usd + entry_fee;
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
                self.cash += notional_usd - entry_fee;
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
                entry_notional: notional_usd,
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
        quote: f64,
        liquidity_usd: f64,
        reason: ExitReason,
        now_secs: u64,
    ) -> Result<ClosedTrade, PortfolioError> {
        self.settle(id, 1.0, quote, liquidity_usd, reason, now_secs)
    }

    /// Close a fraction.
    pub fn close_partial_position(
        &mut self,
        id: PositionId,
        fraction: f64,
        quote: f64,
        liquidity_usd: f64,
        reason: ExitReason,
        now_secs: u64,
    ) -> Result<ClosedTrade, PortfolioError> {
        if !fraction.is_finite() || fraction <= 0.0 || fraction > 1.0 {
            return Err(PortfolioError::InvalidFraction(fraction));
        }
        self.settle(id, fraction, quote, liquidity_usd, reason, now_secs)
    }

    fn settle(
        &mut self,
        id: PositionId,
        fraction: f64,
        quote: f64,
        liquidity_usd: f64,
        reason: ExitReason,
        now_secs: u64,
    ) -> Result<ClosedTrade, PortfolioError> {
        if !quote.is_finite() || quote <= 0.0 {
            return Err(PortfolioError::InvalidPrice(quote));
        }
        // Execution pricing guard: long exits past the curve have no
        // price (unbounded impact). Peek before mutating.
        {
            let position = self
                .positions
                .get(&id)
                .ok_or(PortfolioError::PositionNotFound(id))?;
            let exit_size = (position.quantity.abs() * fraction).abs() * quote;
            let probe = self.fill_for_mint(&position.mint).try_fill_price(
                flip_side(position.side),
                quote,
                exit_size,
                liquidity_usd,
            );
            if probe.is_none() {
                return Err(PortfolioError::CurveExhausted {
                    notional: exit_size,
                    liquidity: liquidity_usd,
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
                quote,
                liquidity_usd,
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
        let trade = self.book(&slice, quote, liquidity_usd, reason, now_secs)?;
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
        quote: f64,
        liquidity_usd: f64,
        reason: ExitReason,
        now_secs: u64,
    ) -> Result<ClosedTrade, PortfolioError> {
        // Execution pricing, uncapped: the settle probe above priced
        // the identical notional, so this agrees with the trigger by
        // construction and can only fail on buy-side exhaustion.
        let exit_notional_probe = slice.quantity.abs() * quote;
        let exit_price = self
            .fill_for_mint(&slice.mint)
            .try_fill_price(
                flip_side(slice.side),
                quote,
                exit_notional_probe,
                liquidity_usd,
            )
            .ok_or(PortfolioError::CurveExhausted {
                notional: exit_notional_probe,
                liquidity: liquidity_usd,
            })?;
        let exit_notional = slice.quantity.abs() * exit_price;
        let exit_fee = self.fee_for(exit_notional);

        let gross_pnl = (exit_price - slice.entry_price) * slice.quantity;
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
        quote: f64,
        liquidity_usd: f64,
        now_secs: u64,
    ) -> Option<ExitReason> {
        let mint = self.positions.get(&id)?.mint.clone();
        let fill = self.fill_for_mint(&mint);
        let position = self.positions.get_mut(&id)?;
        let exit_notional = position.quantity.abs() * quote;
        crate::market::check_exit_realizable(
            &fill,
            position,
            quote,
            exit_notional,
            liquidity_usd,
            now_secs,
        )
    }

    /// Net PnL of closed trades.
    pub fn realized_pnl(&self) -> f64 {
        self.realized_sum
    }

    /// Mark-to-market snapshot.
    pub fn snapshot(&self, marks: &HashMap<String, Mark>) -> PortfolioSnapshot {
        let mut locked = 0.0;
        let mut unrealized = 0.0;
        for position in self.positions.values() {
            let basis = match position.side {
                Side::Long => position.entry_notional,
                Side::Short => -position.entry_notional,
            };
            locked += basis;
            let value = match marks.get(&position.mint) {
                Some(mark) if mark.price.is_finite() && mark.price > 0.0 => {
                    let exit_size = position.quantity.abs() * mark.price;
                    // Same uncapped execution pricing as settlement, so
                    // marks and books agree. Unpriceable (buy-side
                    // exhaustion) holds the basis instead of a fiction.
                    match self.fill_for_mint(&position.mint).try_slip_fraction_for_side(
                        flip_side(position.side),
                        exit_size,
                        mark.liquidity_usd,
                    ) {
                        Some(slip) => {
                            let exit_price = match position.side {
                                Side::Long => mark.price * (1.0 - slip).max(0.0),
                                Side::Short => mark.price * (1.0 + slip),
                            };
                            let exit_notional = position.quantity.abs() * exit_price;
                            let exit_fee = self.fee_for(exit_notional);
                            match position.side {
                                Side::Long => exit_notional - exit_fee,
                                Side::Short => -(exit_notional + exit_fee),
                            }
                        }
                        None => basis,
                    }
                }
                _ => basis,
            };
            unrealized += value - basis;
        }
        PortfolioSnapshot {
            cash: self.cash,
            locked,
            equity: self.cash + locked + unrealized,
            realized_pnl: self.realized_pnl(),
            unrealized_pnl: unrealized,
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
                quote: 1.0,
                notional_usd: 40.0,
                liquidity_usd: 1_000_000.0,
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
            .close_partial_position(id, 0.8, 1.7, 1_000_000.0, ExitReason::TakeProfit, 1001)
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
                quote: 1.0,
                notional_usd: 10.0,
                liquidity_usd: 1_000_000.0,
                tp_percent: 60.0,
                sl_percent: -8.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 90,
                moonbag_percent: 0.0,
                now_secs: 1001,
            })
            .expect("open2");
        pf.close_position(id2, 2.0, 1_000_000.0, ExitReason::TakeProfit, 1002)
            .expect("full close");
        assert!(pf.position(id2).is_none());
    }

    #[test]
    fn open_position_params_matches_shim() {
        let mut via_shim = funded();
        let mut via_struct = funded();
        let params = OpenPositionParams {
            mint: "MINT",
            side: Side::Long,
            quote: 1.0,
            notional_usd: 40.0,
            liquidity_usd: 1_000_000.0,
            tp_percent: 75.0,
            sl_percent: -8.0,
            trailing: false,
            trail_percent: 0.0,
            timeout_seconds: 0,
            moonbag_percent: 0.0,
            now_secs: 1000,
        };
        let a = via_shim
            .open_position(
                params.mint,
                params.side,
                params.quote,
                params.notional_usd,
                params.liquidity_usd,
                params.tp_percent,
                params.sl_percent,
                params.trailing,
                params.trail_percent,
                params.timeout_seconds,
                params.moonbag_percent,
                params.now_secs,
            )
            .expect("shim open");
        let b = via_struct
            .open_position_params(&params)
            .expect("struct open");
        assert_eq!(a, b);
        assert_eq!(
            via_shim.position(a).expect("a").entry_price,
            via_struct.position(b).expect("b").entry_price
        );
        assert_eq!(via_shim.cash(), via_struct.cash());
    }

    #[test]
    fn open_debits_cash_and_close_credits_pnl() {
        let mut pf = funded();
        let id = pf
            .open_position(
                "MINT",
                Side::Long,
                1.0,
                40.0,
                1_000_000.0,
                75.0,
                -8.0,
                false,
                0.0,
                0,
                0.0,
                1000,
            )
            .expect("open");
        assert_eq!(pf.cash(), 60.0);

        let trade = pf
            .close_position(id, 2.0, 1_000_000.0, ExitReason::Manual, 1100)
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
            .open_position(
                "MINT",
                Side::Long,
                1.0,
                50.0,
                1e9,
                75.0,
                -8.0,
                false,
                0.0,
                0,
                0.0,
                0,
            )
            .expect("open");
        assert!((pf.cash() - 49.50).abs() < 1e-9);
        let trade = pf
            .close_position(id, 1.0, 1e9, ExitReason::Manual, 1)
            .expect("close");
        assert!((trade.fees - 1.0).abs() < 1e-9);
        assert!((trade.net_pnl + 1.0).abs() < 1e-9);
        assert!((pf.cash() - 99.0).abs() < 1e-9);
    }

    #[test]
    fn short_roundtrip_flat_price_nets_minus_fees() {
        let mut pf = PaperPortfolio::new(100.0, 100.0).with_fill_model(FillModel::Exact);
        let id = pf
            .open_position(
                "MINT",
                Side::Short,
                1.0,
                50.0,
                1e9,
                75.0,
                -8.0,
                false,
                0.0,
                0,
                0.0,
                0,
            )
            .expect("open");
        assert!((pf.cash() - 149.50).abs() < 1e-9, "cash={}", pf.cash());
        assert_eq!(pf.position(id).expect("p").quantity, -50.0);
        let trade = pf
            .close_position(id, 1.0, 1e9, ExitReason::Manual, 1)
            .expect("close");
        assert!((trade.gross_pnl).abs() < 1e-9, "gross={}", trade.gross_pnl);
        assert!((trade.fees - 1.0).abs() < 1e-9, "fees={}", trade.fees);
        assert!((trade.net_pnl + 1.0).abs() < 1e-9, "net={}", trade.net_pnl);
        assert!((pf.cash() - 99.0).abs() < 1e-9, "cash={}", pf.cash());
        let mut pf2 = PaperPortfolio::new(100.0, 100.0).with_fill_model(FillModel::Exact);
        let id2 = pf2
            .open_position(
                "MINT",
                Side::Short,
                1.0,
                50.0,
                1e9,
                75.0,
                -8.0,
                false,
                0.0,
                0,
                0.0,
                0,
            )
            .expect("open");
        let mut marks = HashMap::new();
        marks.insert("MINT".to_string(), Mark::price_only(1.0));
        let snap = pf2.snapshot(&marks);
        let trade2 = pf2
            .close_position(id2, 1.0, 1e9, ExitReason::Manual, 1)
            .expect("close");
        assert!((pf2.cash() - snap.equity).abs() < 1e-9);
        assert!((trade2.net_pnl + 1.0).abs() < 1e-9);
    }

    #[test]
    fn rejects_oversize_and_bad_inputs() {
        let mut pf = funded();
        assert!(matches!(
            pf.open_position(
                "M",
                Side::Long,
                1.0,
                200.0,
                1e9,
                1.0,
                -1.0,
                false,
                0.0,
                0,
                0.0,
                0
            ),
            Err(PortfolioError::InsufficientFunds { .. })
        ));
        assert!(matches!(
            pf.open_position(
                "M",
                Side::Long,
                0.0,
                10.0,
                1e9,
                1.0,
                -1.0,
                false,
                0.0,
                0,
                0.0,
                0
            ),
            Err(PortfolioError::InvalidPrice(_))
        ));
        assert!(matches!(
            pf.open_position(
                "M",
                Side::Long,
                1.0,
                -5.0,
                1e9,
                1.0,
                -1.0,
                false,
                0.0,
                0,
                0.0,
                0
            ),
            Err(PortfolioError::InvalidNotional(_))
        ));
        assert!(matches!(
            pf.close_position(999, 1.0, 1e9, ExitReason::Manual, 0),
            Err(PortfolioError::PositionNotFound(999))
        ));
    }

    #[test]
    fn slippage_shrinks_size() {
        let mut pf = PaperPortfolio::new(100.0, 0.0);
        let id = pf
            .open_position(
                "MINT",
                Side::Long,
                1.0,
                50.0,
                5_000.0,
                75.0,
                -8.0,
                false,
                0.0,
                0,
                0.0,
                0,
            )
            .expect("open");
        let position = pf.position(id).expect("position");
        assert!(position.entry_price > 1.0);
        assert!(position.quantity < 50.0);
    }

    #[test]
    fn snapshot_marks_unrealized() {
        let mut pf = funded();
        pf.open_position(
            "A",
            Side::Long,
            1.0,
            40.0,
            1e9,
            75.0,
            -8.0,
            false,
            0.0,
            0,
            0.0,
            0,
        )
        .expect("open");
        let mut marks = HashMap::new();
        marks.insert("A".to_string(), Mark { price: 1.5, liquidity_usd: 1e9 });
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
            .open_position("A", Side::Long, 1.0, 40.0, 5_000.0, 75.0, -8.0, false, 0.0, 0, 0.0, 0)
            .expect("open");
        let mut marks = HashMap::new();
        marks.insert("A".to_string(), Mark { price: 1.5, liquidity_usd: 5_000.0 });
        let snap = pf.snapshot(&marks);
        let naive = (1.5 - pf.position(id).expect("p").entry_price)
            * pf.position(id).expect("p").quantity;
        assert!(snap.unrealized_pnl < naive);
        let trade = pf
            .close_position(id, 1.5, 5_000.0, ExitReason::Manual, 1)
            .expect("close");
        assert!((trade.net_pnl - (snap.unrealized_pnl - 0.4)).abs() < 1e-9);
        assert!((pf.cash() - snap.equity).abs() < 1e-9);
        let mut pf2 = PaperPortfolio::new(100.0, 0.0);
        pf2
            .open_position("B", Side::Long, 1.0, 10.0, 1e9, 75.0, -8.0, false, 0.0, 0, 0.0, 0)
            .expect("open");
        let empty: HashMap<String, Mark> = HashMap::new();
        assert_eq!(pf2.snapshot(&empty).unrealized_pnl, 0.0);
        let mut nan_marks = HashMap::new();
        nan_marks.insert("B".to_string(), Mark { price: f64::NAN, liquidity_usd: 5_000.0 });
        assert_eq!(pf2.snapshot(&nan_marks).unrealized_pnl, 0.0);
    }

    #[test]
    fn full_poll_loop_tp_exit() {
        let mut pf = funded();
        let id = pf
            .open_position(
                "MINT",
                Side::Long,
                1.0,
                50.0,
                1e9,
                75.0,
                -8.0,
                false,
                0.0,
                0,
                0.0,
                0,
            )
            .expect("open");
        let reason = {
            let position = pf.position_mut(id).expect("position");
            check_exit(position, 1.80, 10)
        };
        assert_eq!(reason, Some(ExitReason::TakeProfit));
        let trade = pf
            .close_position(id, 1.80, 1e9, reason.unwrap_or(ExitReason::Manual), 10)
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
                .open_position(
                    "M",
                    Side::Long,
                    1.0,
                    10.0,
                    1e9,
                    75.0,
                    -8.0,
                    false,
                    0.0,
                    0,
                    0.0,
                    i,
                )
                .expect("open");
            pf.close_position(id, 0.9, 1e9, ExitReason::StopLoss, i + 1)
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
                    pf.open_position("M", Side::Long, 1.0, 10.0, 1e9, tp, sl, false, 0.0, 0, 0.0, 0),
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
        pf.open_position(
            "MINT",
            Side::Long,
            1.0,
            40.0,
            1e9,
            75.0,
            -8.0,
            false,
            0.0,
            0,
            0.0,
            1000,
        )
        .expect("open")
    }

    #[test]
    fn partial_80_20_split_matches_full_close_totals() {
        let mut pf = funded();
        let id = open_40_at_1(&mut pf);
        let part = pf
            .close_partial_position(id, 0.8, 2.0, 1e9, ExitReason::TakeProfit, 1100)
            .expect("partial");
        assert_eq!(part.fraction_closed, 0.8);
        assert_eq!(part.quantity, 32.0);
        assert_eq!(part.gross_pnl, 32.0);
        assert_eq!(part.net_pnl, 32.0);
        assert_eq!(part.net_pnl_percent, 100.0);
        assert_eq!(pf.cash(), 60.0 + 32.0 + 32.0);

        let open = pf.position(id).expect("runner");
        assert_eq!((open.quantity, open.entry_notional), (8.0, 8.0));
        assert_eq!((open.tp_percent, open.sl_percent), (75.0, 0.0));

        let rest = pf
            .close_partial_position(id, 1.0, 2.0, 1e9, ExitReason::TakeProfit, 1200)
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
        pf.close_partial_position(id, 0.8, 2.0, 1e9, ExitReason::TakeProfit, 1)
            .expect("partial");
        assert_eq!(pf.position(id).expect("runner").sl_percent, 0.0);
    }

    #[test]
    fn ratchet_never_loosens_and_ignores_other_reasons() {
        let mut pf = funded();
        let id = open_40_at_1(&mut pf);
        pf.position_mut(id).expect("p").sl_percent = 5.0;
        pf.close_partial_position(id, 0.5, 2.0, 1e9, ExitReason::TakeProfit, 1)
            .expect("partial");
        assert_eq!(pf.position(id).expect("p").sl_percent, 5.0);

        let id2 = open_40_at_1(&mut pf);
        pf.close_partial_position(id2, 0.5, 2.0, 1e9, ExitReason::Manual, 1)
            .expect("partial");
        assert_eq!(pf.position(id2).expect("p").sl_percent, -8.0);
    }

    #[test]
    fn manual_partial_preserves_runner_terms() {
        let mut pf = funded();
        let id = pf
            .open_position("M", Side::Long, 1.0, 10.0, 1e9, 75.0, -8.0, true, 5.0, 90, 20.0, 0)
            .expect("open");
        pf.close_partial_position(id, 0.5, 1.5, 1e9, ExitReason::Manual, 1)
            .expect("partial");
        let p = pf.position(id).expect("runner");
        assert_eq!(p.sl_percent, -8.0);
        assert_eq!(p.timeout_seconds, 90);
        assert_eq!(p.moonbag_percent, 20.0);
        pf.close_partial_position(id, 0.5, 2.0, 1e9, ExitReason::TakeProfit, 2)
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
            pf.open_position("", Side::Long, 1.0, 10.0, 1e9, 75.0, -8.0, false, 0.0, 0, 0.0, 0),
            Err(PortfolioError::InvalidMint(_))
        ));
    }

    #[test]
    fn nan_trail_and_moonbag_normalize_to_zero() {
        let mut pf = funded();
        let id = pf
            .open_position(
                "M", Side::Long, 1.0, 10.0, 1e9, 75.0, -8.0, true, f64::NAN, 90, f64::NAN, 0,
            )
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
            .open_position("STD", Side::Long, 1.0, 100.0, 1e9, 75.0, -8.0, false, 0.0, 0, 0.0, 0)
            .expect("std open");
        let cus_id = pf
            .open_position("CUSTOM", Side::Long, 1.0, 100.0, 1e9, 75.0, -8.0, false, 0.0, 0, 0.0, 0)
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
            marks.insert(
                "MINT".to_string(),
                Mark { price, liquidity_usd: 1e9 },
            );
            let snap = pf.snapshot(&marks);
            assert_eq!(snap.unrealized_pnl, base.unrealized_pnl, "price {price}");
        }
    }

    #[test]
    fn partial_short_splits_signed_quantity() {
        let mut pf = funded();
        let id = pf
            .open_position(
                "MINT",
                Side::Short,
                2.0,
                40.0,
                1e9,
                75.0,
                -8.0,
                false,
                0.0,
                0,
                0.0,
                0,
            )
            .expect("open");
        assert_eq!(pf.position(id).expect("p").quantity, -20.0);
        let part = pf
            .close_partial_position(id, 0.5, 1.0, 1e9, ExitReason::TakeProfit, 1)
            .expect("partial");
        assert_eq!(part.quantity, -10.0);
        assert_eq!(part.gross_pnl, 10.0);
        assert_eq!(pf.position(id).expect("p").quantity, -10.0);
    }

    #[test]
    fn partial_rejects_bad_fractions_and_missing_ids() {
        let mut pf = funded();
        let id = open_40_at_1(&mut pf);
        for bad in [0.0, -0.5, 1.5, f64::NAN, f64::INFINITY] {
            assert!(
                matches!(
                    pf.close_partial_position(id, bad, 2.0, 1e9, ExitReason::Manual, 0),
                    Err(PortfolioError::InvalidFraction(_))
                ),
                "fraction={bad}"
            );
        }
        assert!(matches!(
            pf.close_partial_position(999, 0.5, 2.0, 1e9, ExitReason::Manual, 0),
            Err(PortfolioError::PositionNotFound(999))
        ));
        assert_eq!(pf.position(id).expect("p").quantity, 40.0);
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
            .close_position(id, 2.0, 1e9, ExitReason::Manual, 1100)
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
            .close_partial_position(id, 0.5, 2.0, 1e9, ExitReason::Manual, 1100)
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
            .close_position(id, 2.0, 1e9, ExitReason::Manual, 1100)
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
                quote: 1.0,
                notional_usd: 10.0,
                liquidity_usd: 1e9,
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
            .close_position(id, 1.02, 1e9, ExitReason::Manual, 1)
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
                quote: 1.0,
                notional_usd: 10_000_000.0,
                liquidity_usd: 10_000.0,
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
                        quote: 1.0,
                        notional_usd: 10.0,
                        liquidity_usd: 1e9,
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
                quote: 1.0,
                notional_usd: 100.0,
                liquidity_usd: 1e9,
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
            pf.check_exit_realizable(id, 1.0, 10.0, 1),
            Some(ExitReason::StopLoss)
        );
        let trade = pf
            .close_position(id, 1.0, 10.0, ExitReason::StopLoss, 1)
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
                quote: 1.0,
                notional_usd: 100.0,
                liquidity_usd: 1e9,
                tp_percent: 100.0,
                sl_percent: -50.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 0,
            })
            .expect("open");
        let qty = pf.positions.get(&id).expect("open").quantity.abs();
        let trade = pf
            .close_position(id, 1.0, 200.0, ExitReason::Manual, 1)
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
                    quote: 1.0,
                    notional_usd: 10.0,
                    liquidity_usd: 1e9,
                    tp_percent: 100.0,
                    sl_percent: -50.0,
                    trailing: false,
                    trail_percent: 0.0,
                    timeout_seconds: 0,
                    moonbag_percent: 0.0,
                    now_secs: i as u64,
                })
                .expect("open");
            pf.close_position(id, 1.0, 1e9, ExitReason::Manual, i as u64)
                .expect("close");
        }
        let _ = HashMap::<String, ()>::new();
        assert_eq!(pf.history().len(), MAX_HISTORY_TRADES);
        let snap = pf.snapshot(&HashMap::new());
        assert!(snap.realized_pnl.is_finite());
        assert_eq!(snap.closed_count, n);
        assert_eq!(pf.win_rate(), Some(0.0));
    }
}

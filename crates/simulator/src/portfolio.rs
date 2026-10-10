//! Virtual portfolio: cash ledger, open positions, realized history.
//!
//! Construct with [`PaperPortfolio::new`] (e.g. `$100`), open positions
//! with [`PaperPortfolio::open_position`], close with
//! [`PaperPortfolio::close_position`]. All prices are USD per token;
//! timestamps are caller-provided unix seconds so backtests stay
//! deterministic.

use crate::market::FillModel;
use crate::types::{ClosedTrade, ExitReason, Position, PositionId, Side};
use std::collections::HashMap;
use thiserror::Error;

/// Portfolio failures: no leverage, no partial fills — every violation is
/// an explicit error.
#[derive(Error, Debug, PartialEq)]
pub enum PortfolioError {
    /// Quote is not a positive finite price.
    #[error("invalid price: {0}")]
    InvalidPrice(f64),
    /// Notional must be positive and finite.
    #[error("invalid notional: {0}")]
    InvalidNotional(f64),
    /// Cash balance cannot cover entry notional + entry fee.
    #[error("insufficient funds: need {need:.2}, have {have:.2}")]
    InsufficientFunds {
        /// Required cash.
        need: f64,
        /// Available cash.
        have: f64,
    },
    /// No open position with this id.
    #[error("position not found: {0}")]
    PositionNotFound(PositionId),
}

/// Point-in-time accounting snapshot.
#[derive(Debug, Clone)]
pub struct PortfolioSnapshot {
    /// Free cash in USD.
    pub cash: f64,
    /// Cash locked in open positions (entry notionals).
    pub locked: f64,
    /// Mark-to-market equity = cash + Σ position values.
    pub equity: f64,
    /// Sum of net PnL over all closed trades.
    pub realized_pnl: f64,
    /// Σ (current value − entry notional) over open positions.
    pub unrealized_pnl: f64,
    /// Open position count.
    pub open_count: usize,
    /// Closed trade count.
    pub closed_count: usize,
}

/// Paper-trading portfolio over one virtual balance.
#[derive(Debug)]
pub struct PaperPortfolio {
    starting_balance: f64,
    cash: f64,
    fee_bps: f64,
    fill: FillModel,
    next_id: PositionId,
    positions: HashMap<PositionId, Position>,
    history: Vec<ClosedTrade>,
}

impl PaperPortfolio {
    /// Fund a portfolio: `starting_balance_usd` (e.g. `100.0`) with
    /// `fee_bps` commission per side (e.g. `30.0` = 0.30%).
    pub fn new(starting_balance_usd: f64, fee_bps: f64) -> Self {
        Self {
            starting_balance: starting_balance_usd.max(0.0),
            cash: starting_balance_usd.max(0.0),
            fee_bps: fee_bps.max(0.0),
            fill: FillModel::default(),
            next_id: 1,
            positions: HashMap::new(),
            history: Vec::new(),
        }
    }

    /// Override the fill model (default: linear bonding-curve impact).
    pub fn with_fill_model(mut self, fill: FillModel) -> Self {
        self.fill = fill;
        self
    }

    /// Free cash in USD.
    pub fn cash(&self) -> f64 {
        self.cash
    }

    /// Starting balance in USD.
    pub fn starting_balance(&self) -> f64 {
        self.starting_balance
    }

    /// Open position handles.
    pub fn position_ids(&self) -> Vec<PositionId> {
        self.positions.keys().copied().collect()
    }

    /// Borrow an open position.
    pub fn position(&self, id: PositionId) -> Option<&Position> {
        self.positions.get(&id)
    }

    /// Full closed-trade history in close order.
    pub fn history(&self) -> &[ClosedTrade] {
        &self.history
    }

    /// Fee for a notional at the configured rate.
    fn fee_for(&self, notional: f64) -> f64 {
        notional * self.fee_bps / 10_000.0
    }

    /// Open a long/short at `quote` with `notional_usd` exposure.
    ///
    /// Execution uses the fill model against `liquidity_usd`; entry fee is
    /// charged on the executed notional. Cash is debited `notional + fee`.
    /// `trail_percent` is the giveback that fires the trailing stop when
    /// `trailing` is set (ignored otherwise).
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
        now_secs: u64,
    ) -> Result<PositionId, PortfolioError> {
        if !quote.is_finite() || quote <= 0.0 {
            return Err(PortfolioError::InvalidPrice(quote));
        }
        if !notional_usd.is_finite() || notional_usd <= 0.0 {
            return Err(PortfolioError::InvalidNotional(notional_usd));
        }

        let entry_price = self
            .fill
            .fill_price(side, quote, notional_usd, liquidity_usd);
        let entry_fee = self.fee_for(notional_usd);
        let need = notional_usd + entry_fee;
        if need > self.cash {
            return Err(PortfolioError::InsufficientFunds {
                need,
                have: self.cash,
            });
        }

        // Quantity follows executed notional so slippage is accounted in size.
        let quantity = match side {
            Side::Long => notional_usd / entry_price,
            Side::Short => -(notional_usd / entry_price),
        };

        self.cash -= need;
        let id = self.next_id;
        self.next_id += 1;
        self.positions.insert(
            id,
            Position {
                id,
                mint: mint.to_string(),
                side,
                entry_price,
                quantity,
                entry_notional: notional_usd,
                entry_fee,
                tp_percent,
                sl_percent,
                trail_percent: trail_percent.max(0.0),
                trailing_stop: trailing,
                best_price: entry_price,
                opened_at: now_secs,
                timeout_seconds,
            },
        );
        Ok(id)
    }

    /// Close a position at `quote` (exit also runs through the fill model).
    ///
    /// Proceeds = exit notional − exit fee are credited back to cash.
    pub fn close_position(
        &mut self,
        id: PositionId,
        quote: f64,
        liquidity_usd: f64,
        reason: ExitReason,
        now_secs: u64,
    ) -> Result<ClosedTrade, PortfolioError> {
        if !quote.is_finite() || quote <= 0.0 {
            return Err(PortfolioError::InvalidPrice(quote));
        }
        let position = self
            .positions
            .remove(&id)
            .ok_or(PortfolioError::PositionNotFound(id))?;

        let exit_price =
            self.fill
                .fill_price(position.side, quote, position.entry_notional, liquidity_usd);
        let exit_notional = position.quantity.abs() * exit_price;
        let exit_fee = self.fee_for(exit_notional);

        let gross_pnl = (exit_price - position.entry_price) * position.quantity;
        let fees = position.entry_fee + exit_fee;
        let net_pnl = gross_pnl - fees;
        let net_pnl_percent = if position.entry_notional > 0.0 {
            net_pnl / position.entry_notional * 100.0
        } else {
            0.0
        };

        // Cash locked at entry was notional + entry fee; return value minus exit fee.
        self.cash += position.entry_notional + gross_pnl - exit_fee;

        let trade = ClosedTrade {
            id,
            mint: position.mint,
            side: position.side,
            entry_price: position.entry_price,
            exit_price,
            quantity: position.quantity,
            gross_pnl,
            fees,
            net_pnl,
            net_pnl_percent,
            reason,
            opened_at: position.opened_at,
            closed_at: now_secs,
            hold_seconds: now_secs.saturating_sub(position.opened_at),
        };
        self.history.push(trade.clone());
        Ok(trade)
    }

    /// Mutable access for [`check_exit`](crate::check_exit)-style polling.
    pub fn position_mut(&mut self, id: PositionId) -> Option<&mut Position> {
        self.positions.get_mut(&id)
    }

    /// Sum of net PnL over closed trades.
    pub fn realized_pnl(&self) -> f64 {
        self.history.iter().map(|t| t.net_pnl).sum()
    }

    /// Mark-to-market snapshot. `quotes` maps mint → current price; missing
    /// mints are marked at entry (zero unrealized).
    pub fn snapshot(&self, quotes: &HashMap<String, f64>) -> PortfolioSnapshot {
        let mut locked = 0.0;
        let mut unrealized = 0.0;
        for position in self.positions.values() {
            locked += position.entry_notional;
            let mark = quotes
                .get(&position.mint)
                .copied()
                .unwrap_or(position.entry_price);
            unrealized += (mark - position.entry_price) * position.quantity;
        }
        PortfolioSnapshot {
            cash: self.cash,
            locked,
            equity: self.cash + locked + unrealized,
            realized_pnl: self.realized_pnl(),
            unrealized_pnl: unrealized,
            open_count: self.positions.len(),
            closed_count: self.history.len(),
        }
    }

    /// Win rate over closed trades (`None` when nothing closed yet).
    pub fn win_rate(&self) -> Option<f64> {
        if self.history.is_empty() {
            return None;
        }
        let wins = self.history.iter().filter(|t| t.net_pnl > 0.0).count();
        Some(wins as f64 / self.history.len() as f64)
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
                0,
            )
            .expect("open");
        // Entry fee 1% of 50 = 0.50; cash = 100 - 50.50.
        assert!((pf.cash() - 49.50).abs() < 1e-9);
        let trade = pf
            .close_position(id, 1.0, 1e9, ExitReason::Manual, 1)
            .expect("close");
        // Flat price: gross 0, fees 0.50 + 0.50.
        assert!((trade.fees - 1.0).abs() < 1e-9);
        assert!((trade.net_pnl + 1.0).abs() < 1e-9);
        assert!((pf.cash() - 99.0).abs() < 1e-9);
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
                0
            ),
            Err(PortfolioError::InsufficientFunds { .. })
        ));
        assert!(matches!(
            pf.open_position("M", Side::Long, 0.0, 10.0, 1e9, 1.0, -1.0, false, 0.0, 0, 0),
            Err(PortfolioError::InvalidPrice(_))
        ));
        assert!(matches!(
            pf.open_position("M", Side::Long, 1.0, -5.0, 1e9, 1.0, -1.0, false, 0.0, 0, 0),
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
                0,
            )
            .expect("open");
        let position = pf.position(id).expect("position");
        // 1% of liquidity at 50 bps/1% -> fill 1.005, fewer tokens.
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
            0,
        )
        .expect("open");
        let mut quotes = HashMap::new();
        quotes.insert("A".to_string(), 1.5);
        let snap = pf.snapshot(&quotes);
        assert_eq!(snap.cash, 60.0);
        assert_eq!(snap.locked, 40.0);
        assert_eq!(snap.unrealized_pnl, 20.0);
        assert_eq!(snap.equity, 120.0);
        assert_eq!(snap.open_count, 1);
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
    fn move_percent_degenerate_is_zero() {
        assert_eq!(move_percent(Side::Long, 0.0, 1.0), 0.0);
    }
}

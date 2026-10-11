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
    /// Take-profit must be a positive percent (would fire instantly).
    #[error("invalid take-profit: {0}")]
    InvalidTakeProfit(f64),
    /// Stop-loss must be a negative percent (would fire instantly).
    #[error("invalid stop-loss: {0}")]
    InvalidStopLoss(f64),
    /// Close fraction must satisfy `0 < fraction <= 1` (e.g. `0.8`: exit
    /// 80%, keep a 20% moonbag runner).
    #[error("invalid close fraction: {0}")]
    InvalidFraction(f64),
    /// Mint must be a non-empty address string.
    #[error("invalid mint: {0:?}")]
    InvalidMint(String),
}

/// Pro-rata slice of a position being settled. Owns its data so the
/// ledger can price it while the open position is borrowed separately.
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
    /// Whole position (full close).
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

    /// `fraction` share (`0 < fraction < 1`; caller validates).
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

/// Opposite side: exits always trade against the entry direction.
fn flip_side(side: Side) -> Side {
    match side {
        Side::Long => Side::Short,
        Side::Short => Side::Long,
    }
}

/// Finite `v`, else disarmed `0.0` (for trail/moonbag normalization:
/// NaN would poison downstream exit math through `clamp`/`max`).
fn finite_or_zero(v: f64) -> f64 {
    if v.is_finite() { v } else { 0.0 }
}

/// Mark-to-market quote for one mint: price plus the liquidity context
/// needed to estimate exit costs.
#[derive(Debug, Clone, Copy)]
pub struct Mark {
    /// Current price in USD per token.
    pub price: f64,
    /// Available liquidity in USD (fee-only estimate when unknown).
    pub liquidity_usd: f64,
}

impl Mark {
    /// Price without liquidity context (exit-cost estimate is fee-only).
    pub fn price_only(price: f64) -> Self {
        Self { price, liquidity_usd: f64::NAN }
    }
}

/// All inputs for opening a position, bundled so callers cannot
/// transpose arguments and new knobs don't grow the arity.
/// Prefer [`PaperPortfolio::open_position_params`] for new code;
/// [`PaperPortfolio::open_position`] is a thin shim over it.
#[derive(Debug, Clone)]
pub struct OpenPositionParams<'a> {
    /// Token mint (base58).
    pub mint: &'a str,
    /// Direction (`Long` for spot snipes; `Short` exists for PnL-math generality).
    pub side: Side,
    /// Current quote in USD per token.
    pub quote: f64,
    /// Exposure in USD.
    pub notional_usd: f64,
    /// Pool liquidity in USD (slippage context).
    pub liquidity_usd: f64,
    /// Take-profit in percent (> 0).
    pub tp_percent: f64,
    /// Stop-loss in percent (< 0).
    pub sl_percent: f64,
    /// Whether the trailing stop is armed.
    pub trailing: bool,
    /// Trailing giveback in percent (ignored unless `trailing`).
    pub trail_percent: f64,
    /// Time-stop in seconds (0 = none).
    pub timeout_seconds: u64,
    /// Share kept riding after a TakeProfit scale-out (0–100, clamped).
    pub moonbag_percent: f64,
    /// Now in unix seconds (hold-time accounting).
    pub now_secs: u64,
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
    ///
    /// Two fee layers apply — do not confuse them: `fee_bps` is the
    /// *commission* charged here on entry/exit notionals, while the
    /// [`FillModel::BondingCurve`] fill prices already embed the *venue*
    /// fee (125 bps/side on Pump.fun) plus price impact. Setting
    /// `fee_bps = 125` on top of the default fill model would
    /// double-count the venue fee; keep `fee_bps` to commission only.
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

    /// Override the fill model (default: bonding-curve cost model).
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

    /// Open position handles, in arbitrary map order (sort when
    /// deterministic order matters).
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

    /// Handles of open positions for one mint (no intermediate alloc
    /// beyond the returned ids).
    pub fn positions_for_mint(&self, mint: &str) -> Vec<PositionId> {
        self.positions
            .iter()
            .filter(|(_, p)| p.mint == mint)
            .map(|(id, _)| *id)
            .collect()
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
    /// `trailing` is set (ignored otherwise). `moonbag_percent` (0–100,
    /// clamped) is the share kept riding after a TakeProfit scale-out.
    ///
    /// Thin shim over [`Self::open_position_params`]; kept so existing
    /// callers don't churn. New code should pass
    /// [`OpenPositionParams`] directly.
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

    /// Open a position from [`OpenPositionParams`] (preferred entry point).
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
        if !sl_percent.is_finite() || sl_percent >= 0.0 {
            return Err(PortfolioError::InvalidStopLoss(sl_percent));
        }
        if p.mint.is_empty() {
            return Err(PortfolioError::InvalidMint(p.mint.to_string()));
        }

        let entry_price = self
            .fill
            .fill_price(p.side, quote, notional_usd, p.liquidity_usd);
        let entry_fee = self.fee_for(notional_usd);
        let need = notional_usd + entry_fee;
        if need > self.cash {
            return Err(PortfolioError::InsufficientFunds {
                need,
                have: self.cash,
            });
        }

        // Quantity follows executed notional so slippage is accounted in size.
        let quantity = match p.side {
            Side::Long => notional_usd / entry_price,
            Side::Short => -(notional_usd / entry_price),
        };

        self.cash -= need;
        let id = self.next_id;
        // 2^64 positions is unreachable; a silent saturating reuse would
        // overwrite a live position, so fail loudly instead (precedent:
        // Borsh-encode expects elsewhere in the tree).
        self.next_id = self.next_id.checked_add(1).expect("position id space exhausted");
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
                // Non-finite trail/moonbag normalize to disarmed (0):
                // NaN clamps to NaN and would poison downstream math.
                trail_percent: finite_or_zero(p.trail_percent).max(0.0),
                trailing_stop: p.trailing,
                best_price: entry_price,
                opened_at: p.now_secs,
                timeout_seconds: p.timeout_seconds,
                moonbag_percent: finite_or_zero(p.moonbag_percent).clamp(0.0, 100.0),
            },
        );
        Ok(id)
    }

    /// Close a position at `quote` (exit also runs through the fill model).
    ///
    /// Proceeds = exit notional − exit fee are credited back to cash.
    /// Equivalent to [`PaperPortfolio::close_partial_position`] with
    /// `fraction = 1.0`.
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

    /// Close `fraction` of a position (`0 < fraction <= 1`, e.g. `0.8`
    /// exits 80% and leaves a 20% moonbag runner with identical entry
    /// terms). The closed slice is accounted proportionally (notional,
    /// quantity, entry fee); the remainder keeps running with its
    /// original TP/trailing state, except a TakeProfit scale-out ratchets
    /// its stop to breakeven (`max(sl, 0.0)` — never loosened).
    /// `fraction == 1.0` removes the position exactly like
    /// [`PaperPortfolio::close_position`].
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

    /// Shared settlement: price the `fraction` slice through the fill
    /// model, credit cash, shrink or remove the position, record history.
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
            );
            self.history.push(trade.clone());
            return Ok(trade);
        }
        let slice = {
            let position = self
                .positions
                .get(&id)
                .ok_or(PortfolioError::PositionNotFound(id))?;
            CloseSlice::part(position, fraction)
        };
        let trade = self.book(&slice, quote, liquidity_usd, reason, now_secs);
        if let Some(open) = self.positions.get_mut(&id) {
            open.quantity -= slice.quantity;
            open.entry_notional -= slice.entry_notional;
            open.entry_fee -= slice.entry_fee;
            // Moonbag protection, TakeProfit scale-outs ONLY: the runner's
            // stop ratchets to breakeven (never loosens an already-tighter
            // stop) and the time-stop lifts plus the moonbag disarms so it
            // rides on trailing alone. A Manual trim must not do any of
            // this — otherwise a 1% rebalance accidentally creates a
            // never-expiring, never-scalable position.
            //
            // Note the ratchet is move-breakeven, not net-breakeven:
            // entry+exit fees and slippage still apply, so a stopped
            // runner nets a small bounded loss by design.
            if reason == ExitReason::TakeProfit && fraction < 1.0 {
                open.sl_percent = open.sl_percent.max(0.0);
                open.timeout_seconds = 0;
                open.moonbag_percent = 0.0;
            }
        }
        self.history.push(trade.clone());
        Ok(trade)
    }

    /// Price one slice and credit its proceeds to cash.
    fn book(
        &mut self,
        slice: &CloseSlice,
        quote: f64,
        liquidity_usd: f64,
        reason: ExitReason,
        now_secs: u64,
    ) -> ClosedTrade {
        let exit_price = self.fill.fill_price(
            flip_side(slice.side),
            quote,
            slice.quantity.abs() * quote,
            liquidity_usd,
        );
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

        // Cash locked at entry was notional + entry fee; return value minus exit fee.
        self.cash += slice.entry_notional + gross_pnl - exit_fee;

        ClosedTrade {
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
        }
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
    /// Mark-to-market snapshot. Each open position is valued at its
    /// estimated exit proceeds (mark price through the fill model minus
    /// exit commission), so `equity` already reflects the cost of
    /// liquidating. Mints without marks contribute zero unrealized.
    pub fn snapshot(&self, marks: &HashMap<String, Mark>) -> PortfolioSnapshot {
        let mut locked = 0.0;
        let mut unrealized = 0.0;
        for position in self.positions.values() {
            locked += position.entry_notional;
            let value = match marks.get(&position.mint) {
                Some(mark) if mark.price.is_finite() && mark.price > 0.0 => {                    let exit_size = position.quantity.abs() * mark.price;
                    let slip = self.fill.slip_fraction(exit_size, mark.liquidity_usd);
                    let exit_price = match position.side {
                        Side::Long => mark.price * (1.0 - slip),
                        Side::Short => mark.price * (1.0 + slip),
                    };
                    let gross = (exit_price - position.entry_price) * position.quantity;
                    let exit_fee =
                        self.fee_for(position.quantity.abs() * exit_price);
                    position.entry_notional + gross - exit_fee
                }
                _ => position.entry_notional,
            };
            unrealized += value - position.entry_notional;
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
    fn moonbag_scale_out_lifts_timeout_and_disarms() {
        // Runner freedom: after a partial TP exit the survivor has no
        // time-stop (rides to trailing/SL/TP) and cannot scale out twice.
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
        assert_eq!(runner.timeout_seconds, 0);
        assert_eq!(runner.moonbag_percent, 0.0);
        assert_eq!(runner.sl_percent, 0.0);
        assert!(runner.trailing_stop);
        // Full closes still remove the position (no survivor to lift).
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
        // Locks the 12-arg shim to the struct path: same inputs must yield
        // the same position, so future callers can migrate without drift.
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
        // Default model + 1% commission: snapshot must predict exactly
        // what close_position will realize at the same mark.
        let mut pf = PaperPortfolio::new(100.0, 100.0);
        let id = pf
            .open_position("A", Side::Long, 1.0, 40.0, 5_000.0, 75.0, -8.0, false, 0.0, 0, 0.0, 0)
            .expect("open");
        let mut marks = HashMap::new();
        marks.insert("A".to_string(), Mark { price: 1.5, liquidity_usd: 5_000.0 });
        let snap = pf.snapshot(&marks);
        // Costs deducted: below the naive (mark-entry)*qty figure.
        let naive = (1.5 - pf.position(id).expect("p").entry_price)
            * pf.position(id).expect("p").quantity;
        assert!(snap.unrealized_pnl < naive);
        let trade = pf
            .close_position(id, 1.5, 5_000.0, ExitReason::Manual, 1)
            .expect("close");
        // Unrealized excludes the sunk entry fee (0.4); realized nets it.
        assert!((trade.net_pnl - (snap.unrealized_pnl - 0.4)).abs() < 1e-9);
        assert!((pf.cash() - snap.equity).abs() < 1e-9);
        // Missing marks and NaN marks contribute zero unrealized.
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
        // Scale out 80% at 2x: 32 of 40 notional, gross +32.
        let part = pf
            .close_partial_position(id, 0.8, 2.0, 1e9, ExitReason::TakeProfit, 1100)
            .expect("partial");
        assert_eq!(part.fraction_closed, 0.8);
        assert_eq!(part.quantity, 32.0);
        assert_eq!(part.gross_pnl, 32.0);
        assert_eq!(part.net_pnl, 32.0);
        assert_eq!(part.net_pnl_percent, 100.0);
        assert_eq!(pf.cash(), 60.0 + 32.0 + 32.0);

        // Moonbag runner keeps TP but its stop ratchets to breakeven.
        let open = pf.position(id).expect("runner");
        assert_eq!((open.quantity, open.entry_notional), (8.0, 8.0));
        assert_eq!((open.tp_percent, open.sl_percent), (75.0, 0.0));

        // Exiting the runner settles exactly like an equivalent full close.
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
        // Runner can no longer lose: stop moved from -8% to entry.
        assert_eq!(pf.position(id).expect("runner").sl_percent, 0.0);
    }

    #[test]
    fn ratchet_never_loosens_and_ignores_other_reasons() {
        let mut pf = funded();
        // Already-tight stop survives a TP scale-out untouched.
        let id = open_40_at_1(&mut pf);
        pf.position_mut(id).expect("p").sl_percent = 5.0;
        pf.close_partial_position(id, 0.5, 2.0, 1e9, ExitReason::TakeProfit, 1)
            .expect("partial");
        assert_eq!(pf.position(id).expect("p").sl_percent, 5.0);

        // Manual scale-outs never move stops.
        let id2 = open_40_at_1(&mut pf);
        pf.close_partial_position(id2, 0.5, 2.0, 1e9, ExitReason::Manual, 1)
            .expect("partial");
        assert_eq!(pf.position(id2).expect("p").sl_percent, -8.0);
    }

    #[test]
    fn manual_partial_preserves_runner_terms() {
        // Only TakeProfit scale-outs ratchet the stop, lift the timeout,
        // and disarm the moonbag: a Manual trim keeps all three intact.
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
        // ...while a TakeProfit scale-out transforms all three.
        pf.close_partial_position(id, 0.5, 2.0, 1e9, ExitReason::TakeProfit, 2)
            .expect("partial");
        let p = pf.position(id).expect("runner");
        assert_eq!(p.sl_percent, 0.0);
        assert_eq!(p.timeout_seconds, 0);
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
        // Price halved: short gains (1-2)*(-10) = +10 on half size.
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
        // Position untouched by rejected closes.
        assert_eq!(pf.position(id).expect("p").quantity, 40.0);
        assert_eq!(pf.cash(), 60.0);
    }
}

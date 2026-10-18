//! Live sizing plus Jito dispatch. Plans are pure; trader hits network.

use crate::settings::now_secs;
use crate::wal;
use executor::{
    BundleStatus, BuyOrder, DeadReason, DispatchVerdict, ExecutorError, JitoReceipt, SellOrder,
    SwapExecutor,
};
use futures_util::stream::StreamExt;
use ids::{ATA_RENT_LAMPORTS, BASE_TX_FEE_LAMPORTS};
use simulator::{FillModel, Side};
use solana_pubkey::Pubkey;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;

/// Mint decimals. Confirm verifies 6; else drop.
/// Single source is `curve::PUMP_TOKEN_DECIMALS`.
pub const TOKEN_DECIMALS: u32 = curve::PUMP_TOKEN_DECIMALS as u32;
/// Lamports per SOL. Single source is `curve::layout::LAMPORTS_PER_SOL`.
pub const LAMPORTS_PER_SOL: f64 = curve::layout::LAMPORTS_PER_SOL;
/// Standard fee tier (bps/side). Single source is
/// `simulator::STANDARD_VENUE_FEE_BPS` (f64 domain there).
pub const VENUE_FEE_BPS: u64 = simulator::STANDARD_VENUE_FEE_BPS as u64;
/// Basis-point denominator shared by fee and slippage math.
/// Single source is `executor::quote::BPS_DENOMINATOR` (u128 domain there).
pub const BPS_DENOMINATOR: u64 = executor::quote::BPS_DENOMINATOR as u64;

/// Mark-price trail per mint for volatility-adaptive slippage.
type MarkTrail = std::sync::Arc<std::sync::Mutex<HashMap<String, VecDeque<(u64, f64)>>>>;

/// Live-trade failures.
#[derive(Error, Debug)]
pub enum LiveError {
    /// Bad inputs: {0}
    #[error("bad live order: {0}")]
    BadOrder(String),
    /// Dispatch failed: {0}
    #[error("dispatch failed: {0}")]
    Dispatch(#[from] ExecutorError),
    /// Accepted but never confirmed. `Confirmed failure` means no fill.
    #[error("unconfirmed live fill: {0}")]
    Unconfirmed(String),
    /// Dispatch proven dead before landing. Only `FailedOnChain` cost
    /// money: the chain charged the base + priority fee (the tip
    /// transfer rolls back with the failed transaction); never-forwarded
    /// and expired dispatches paid nothing.
    #[error("dead live dispatch ({reason}): {context}")]
    Dead { reason: DeadReason, context: String },
    /// Timed out with no signature: fill unknown, needs review.
    /// Boxed for `clippy::result_large_err`.
    #[error("unknown dispatch fill: {reason}")]
    UnknownDispatch {
        position: Box<LivePosition>,
        reason: String,
    },
}

/// Confirm deadline per dispatch.
pub const LIVE_CONFIRM_TIMEOUT: Duration = Duration::from_secs(60);
/// Dispatch deadline. Timeout means unknown fill, needs review.
pub const LIVE_DISPATCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Min spacing between sell retries per mint.
pub const SELL_RETRY_COOLDOWN: Duration = Duration::from_secs(30);
/// Failed sells before manual exit.
pub const SELL_MAX_ATTEMPTS: u32 = 5;
/// Max age of a resend-guard entry: a bundle that is still
/// unresolved after this long is either lying status API or ancient
/// history — holding resends forever strands the bag, so the guard
/// expires loudly (warn) instead. Every earlier release goes through
/// the shared resolver (chain evidence first) and is refused without
/// proof; only this last resort is deliberately evidence-free, sized
/// well past blockhash validity (~60-90s), so the bundle cannot land
/// under any normal condition. Jito bundles resolve in minutes; an
/// hour is pure outage margin.
pub const SELL_GUARD_TTL_SECS: u64 = 3_600;
/// Resolver poll deadline for guard checks: one-shot, so a guard
/// check never blocks the exit path waiting for votes.
const SELL_GUARD_RESOLVE_TIMEOUT: Duration = Duration::ZERO;
/// Recheck rounds before manual review.
pub const PENDING_MAX_ATTEMPTS: u32 = 5;
/// Recheck fanout: bounded like the gate (`GATE_FANOUT` there is also 16).
/// An unbounded join spawns one confirm RPC per pending mint at once.
pub const RECHECK_FANOUT: usize = 16;

/// Post-migration pool venue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolVenue {
    /// Raydium AMM V4 (legacy venue).
    RaydiumV4,
    /// PumpSwap AMM (live venue).
    PumpSwap,
}

/// Pool leg for one mint. Vault orientation is WSOL-side-first on
/// both venues (BASE holds WSOL, QUOTE the token). Validated addresses
/// (parsed once at the follow boundary), never re-validated strings.
#[derive(Debug, Clone)]
pub struct PoolLeg {
    pub mint: Pubkey,
    pub amm: Pubkey,
    /// Coin vault (WSOL).
    pub base_vault: Pubkey,
    /// Pc vault (token).
    pub quote_vault: Pubkey,
    /// Routing venue.
    pub venue: PoolVenue,
}

/// Tracked fill (base units). Validated addresses: custody code never
/// re-parses strings to tell its own holdings apart.
#[derive(Debug, Clone)]
pub struct LivePosition {
    pub mint: Pubkey,
    pub curve: Pubkey,
    pub assoc_curve: Pubkey,
    /// Bought units (for full exits).
    pub amount_tokens: u64,
    /// Creator. `None` fails v3 dispatch.
    pub creator: Option<Pubkey>,
}

/// Buy request. All addresses validated (`Pubkey` is `Copy`, so no
/// lifetime); parsing happens once at the caller boundary.
#[derive(Debug, Clone, Copy)]
pub struct LiveBuyParams {
    pub mint: Pubkey,
    pub curve: Pubkey,
    pub assoc_curve: Pubkey,
    /// Exposure in USD.
    pub notional_usd: f64,
    /// SOL price in USD.
    pub sol_usd: f64,
    /// Market state: spot, book liquidity, and (when the confirm tick
    /// carried it) the exact curve reserves. One carrier, so the paper
    /// book, the guard, and live sizing quote the same state.
    pub mark: simulator::Mark,
    /// Fee tier. `None` is standard.
    pub venue_fee_bps: Option<f64>,
    pub creator: Option<Pubkey>,
    /// Mint decimals (confirm gate drops non-6; explicit so sizing
    /// never reads a hidden constant).
    pub decimals: u8,
}

/// Planned buy: amounts ready for dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveBuyPlan {
    pub amount_tokens: u64,
    /// Slippage guard in lamports.
    pub max_sol_cost: u64,
}

/// Curve-leg sell request. Bundled like buys (arity never grows).
#[derive(Debug, Clone, Copy)]
pub struct LiveSellParams<'a> {
    pub position: &'a LivePosition,
    /// Market state at the exit tick: the floor quotes the same spot,
    /// liquidity, and (when present) exact reserves the paper book
    /// settles against.
    pub mark: simulator::Mark,
    pub sol_usd: f64,
    /// Fee tier. `None` is standard.
    pub venue_fee_bps: Option<f64>,
    /// Jito tip (congestion-aware; caller computes via `effective_tip`).
    pub tip_lamports: u64,
    /// Mint decimals (explicit like buys; non-6 fails closed upstream).
    pub decimals: u8,
}

/// Raydium-leg sell request. Floor prices from vault reserves, so no
/// USD inputs cross here (the curve print is gone on this path).
#[derive(Debug, Clone, Copy)]
pub struct RaydiumSellParams<'a> {
    pub position: &'a LivePosition,
    pub pool: &'a PoolLeg,
    /// Jito tip (congestion-aware; caller computes via `effective_tip`).
    pub tip_lamports: u64,
}

/// PumpSwap-leg sell request. Floor quotes from live reserves plus
/// the fee schedule, so no USD inputs cross here either.
#[derive(Debug, Clone, Copy)]
pub struct PumpSwapSellParams<'a> {
    pub position: &'a LivePosition,
    pub pool: &'a PoolLeg,
    /// Jito tip (congestion-aware; caller computes via `effective_tip`).
    pub tip_lamports: u64,
}

/// Canonical test addresses: valid base58 `Pubkey`s shared by every
/// test module that touches custody types (which parse at boundaries).
/// Distinct from dev (7) and buyer (11-15) fixture wallets.
/// `*_A`/`*_B` serve multi-mint tests.
#[cfg(test)]
pub(crate) const TEST_MINT: &str = "32ZsJ2yJjwuoBiWE5xnZjG9tKmK3CubbmEzgkQLyQzgD";
#[cfg(test)]
pub(crate) const TEST_CURVE: &str = "36VASLSKLFD2KokjXG7V28veZvXEsyHRKefLonPaAKzv";
#[cfg(test)]
pub(crate) const TEST_ASSOC: &str = "3AQTaduKvYWFTu1ExZSQK1hQp5jSZ2yEt4KzsASAufKd";
#[cfg(test)]
pub(crate) const TEST_MINT_A: &str = "3mg7sM6RFEBHiiFotFNfvteH1WdFcc9cujKuPaqZdfDz";
#[cfg(test)]
pub(crate) const TEST_MINT_B: &str = "3qbR1eZRqXUWroWKKYhbDmR3FfqTHfqSU8zZSxtANzYh";

/// Deterministic test key (pool legs, vaults — anything not compared
/// against the shared mints above). Seed-distinct from fixture wallets.
#[cfg(test)]
pub(crate) fn test_pubkey(seed: u8) -> Pubkey {
    Pubkey::new_from_array([seed; 32])
}

/// Max tip multiplier on sell retries. Congestion response is bounded:
/// a stuck bag must not burn the wallet, and floors never widen
/// instead (price protection is the policy; landing odds are the knob).
pub const TIP_ESCALATION_MAX_MULT: u64 = 4;

/// Tip for a sell attempt: base x (1 + prior failures), capped. Pure.
pub fn escalated_tip(base_tip_lamports: u64, failed_attempts: u32) -> u64 {
    let mult = failed_attempts
        .saturating_add(1)
        .min(TIP_ESCALATION_MAX_MULT as u32);
    base_tip_lamports.saturating_mul(mult as u64)
}

/// Priority fee in lamports: `limit * price / 1e6`, ceiled. Delegates
/// to the single [`executor::quote::priority_fee_lamports`]
/// implementation so live debiting and the paper estimate share one
/// formula (DRY). Clamped to a sane lamport cap: a giant price must
/// not turn a cost estimate into a multi-SOL debit.
pub fn priority_fee_lamports(compute_unit_limit: u32, micro_lamports: u64) -> u64 {
    executor::quote::priority_fee_lamports(compute_unit_limit, micro_lamports)
}

/// Paper-mode dispatch-bleed estimate per leg (open, close), USD.
/// Mirrors live debiting (first-touch plus ATA rent on open, dispatch
/// on close) so paper-only PnL nets the same bleed live pays.
/// Zero when priceless.
///
/// Absolute lamports, NOT a rate: at defaults (100k tip, SOL $150)
/// the round trip is ~$0.34 — ~0.3% on a $113 ticket but ~3.4% on the
/// default $10 `NOTIONAL_USD`, larger than the 2.47% venue round trip.
/// This is a calm-weather FLOOR, not a ceiling: live pays escalated
/// tips (up to 4x base), the moving Jito `tip_floor` oracle, and the
/// same priority leg under congestion. Paper is cheap exactly when
/// live is expensive; read paper edge net of this floor as optimistic.
pub fn paper_dispatch_legs_usd(
    tip_lamports: u64,
    priority_lamports: u64,
    sol_usd: f64,
) -> (f64, f64) {
    if !sol_usd.is_finite() || sol_usd <= 0.0 {
        return (0.0, 0.0);
    }
    let leg =
        |lamports: u64| lamports as f64 / LAMPORTS_PER_SOL * sol_usd;
    (
        leg(
            tip_lamports
                .saturating_add(priority_lamports)
                .saturating_add(BASE_TX_FEE_LAMPORTS)
                .saturating_add(ATA_RENT_LAMPORTS),
        ),
        leg(
            tip_lamports
                .saturating_add(priority_lamports)
                .saturating_add(BASE_TX_FEE_LAMPORTS),
        ),
    )
}

/// Exact model quote state from a tick's parsed curve account. The one
/// adapter between the wire-side tick (`curve::ExecState`) and the
/// simulator's quote input (`simulator::CurveReserves`): plain field
/// copies, no math (the model owns the quote formula).
pub fn curve_reserves(exec: &curve::ExecState) -> simulator::CurveReserves {
    simulator::CurveReserves {
        vq_lamports: exec.reserves.virtual_sol_reserves,
        vt_base: exec.reserves.virtual_token_reserves,
        real_tokens: exec.reserves.real_token_reserves,
        decimals: exec.decimals,
        sol_usd: exec.sol_usd,
    }
}

/// Market mark for one tick: spot, depth, and the exact curve state
/// when the tick carried parsed reserves. Single tick→model adapter, so
/// paper fills, live sizing, and snapshots price the same state.
pub fn mark_from_tick(tick: &curve::PriceTick) -> simulator::Mark {
    simulator::Mark {
        price: tick.price_usd,
        liquidity_usd: tick.liquidity_usd,
        curve: tick.exec.as_ref().map(curve_reserves),
    }
}

/// Mark for paper settlement on a tick: like [`mark_from_tick`], except
/// a migrated (complete) curve carries no reserves — the live venue is
/// the pool then, and the curve's reserves are a stale snapshot, not a
/// market. Migration closes book the stale curve spot by design
/// (documented boundary; the live pool leg settles at vault reserves).
pub fn paper_mark_from_tick(tick: &curve::PriceTick) -> simulator::Mark {
    let mark = mark_from_tick(tick);
    if tick.complete {
        simulator::Mark {
            curve: None,
            ..mark
        }
    } else {
        mark
    }
}

/// Observed buy fill priced in USD (entry-gap calibration reading).
/// Pure observation — no judgment. The tracker computes the signed
/// gap vs the paper quote (single owner of gap math, like exits).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BuyFill {
    pub fill_price_usd: f64,
    pub tokens: f64,
}

/// Price a chain-observed fill in USD. Pure (unit-pinned): `None` on
/// zero inputs, non-finite/positive rates, or absurd decimals —
/// calibration skips, never divides by zero.
fn buy_fill_price_usd(
    swap_sol_lamports: u64,
    tokens_base_units: u64,
    decimals: u8,
    sol_usd: f64,
) -> Option<f64> {
    if swap_sol_lamports == 0 || tokens_base_units == 0 {
        return None;
    }
    if !sol_usd.is_finite() || sol_usd <= 0.0 {
        return None;
    }
    if decimals > 18 {
        return None;
    }
    let tokens = tokens_base_units as f64 / 10f64.powi(decimals as i32);
    if !tokens.is_finite() || tokens <= 0.0 {
        return None;
    }
    let price = swap_sol_lamports as f64 / LAMPORTS_PER_SOL * sol_usd / tokens;
    if price.is_finite() { Some(price) } else { None }
}

/// Guard over a fee-inclusive quote: quote plus `slippage_bps`, up.
/// `None` on zero or overflow (fail-closed).
pub fn apply_slippage_ceiling(quoted_cost_incl_fees: u64, slippage_bps: u64) -> Option<u64> {
    // One constructor so both sides match.
    executor::quote::slippage_ceiling(quoted_cost_incl_fees, slippage_bps)
}

/// Size a buy from USD notional through the paper fill model.
/// Fee tier is required. `None` on bad inputs or overflow (fail-closed).
/// Float to `u64`, fail-closed outside range.
fn u64_from_f64_floor(v: f64) -> Option<u64> {
    if !v.is_finite() || v <= 0.0 || v >= u64::MAX as f64 {
        return None;
    }
    Some(v as u64)
}

/// Fee-inclusive buy cost in lamports: base plus per-leg ceil.
///
/// Mirrors `executor::quote::buy_cost_incl_fees` (quote.rs:325-328),
/// which ceils the protocol and creator legs separately. The sizing tier
/// here is a single total; `ceil(a) + ceil(b)` exceeds `ceil(a + b)` by
/// at most one lamport, so single-ceil plus one covers every on-chain
/// split without underfunding. `None` on a bad tier or overflow
/// (fail-closed).
fn fee_inclusive_cost(base_lamports: u64, fee_bps_per_side: f64) -> Option<u64> {
    if !fee_bps_per_side.is_finite() || fee_bps_per_side < 0.0 {
        return None;
    }
    let denom = BPS_DENOMINATOR as u128;
    // Tiers are integers on-chain; ceil here matches that domain.
    let fee_bps = fee_bps_per_side.ceil() as u128;
    let single = u128::from(base_lamports)
        .checked_mul(denom.checked_add(fee_bps)?)?
        .div_ceil(denom);
    // Double-ceil cover: on-chain ceils each fee leg separately.
    u64::try_from(single.checked_add(1)?).ok()
}

/// Size a buy from USD notional through the uncapped execution fill.
/// Fee tier is required; `decimals` is the mint's token decimals
/// (callers pass it explicitly — the gate guarantees 6 — so the calc
/// path never reads the constant behind the caller's back). `None` on
/// bad inputs, curve exhaustion, or overflow (fail-closed).
pub fn plan_buy(
    notional_usd: f64,
    sol_usd: f64,
    slippage_bps: u64,
    fee_bps_per_side: f64,
    decimals: u8,
    mark: &simulator::Mark,
) -> Option<LiveBuyPlan> {
    let price_usd = mark.price;
    if !notional_usd.is_finite() || notional_usd <= 0.0 {
        return None;
    }
    if !price_usd.is_finite() || price_usd <= 0.0 {
        return None;
    }
    if !sol_usd.is_finite() || sol_usd <= 0.0 {
        return None;
    }
    if !fee_bps_per_side.is_finite() || fee_bps_per_side < 0.0 {
        return None;
    }
    if decimals > 18 {
        return None;
    }
    // Exact state when the tick carried reserves: the chain's own
    // inverse and cost, identical to the paper book's entry fill (up to
    // whole-token dust) and to the dispatch-time guard. `None` when the
    // curve cannot absorb the ticket — no capped fiction.
    if let Some(curve) = mark.curve {
        let lamports = notional_usd / sol_usd * LAMPORTS_PER_SOL;
        let base = u64_from_f64_floor(lamports)?;
        let amount_tokens = curve.buy_tokens_base(base, fee_bps_per_side)?;
        if amount_tokens == 0 {
            return None;
        }
        let max_sol_cost = apply_slippage_ceiling(
            curve.buy_cost_lamports(amount_tokens, fee_bps_per_side)?,
            slippage_bps,
        )?;
        return Some(LiveBuyPlan {
            amount_tokens,
            max_sol_cost,
        });
    }
    // Proxy basis (tests, synthetic prices): size from the uncapped
    // execution fill. `None` when the curve cannot absorb the size
    // (`notional >= liquidity`, like `PaperPortfolio::open_position_params`
    // -> `CurveExhausted`) — no capped fiction.
    let fill = FillModel::default().with_venue_fee_bps(fee_bps_per_side);
    let slip = fill.try_slip_fraction_for_side(Side::Long, notional_usd, mark.liquidity_usd)?;
    let scale = 10f64.powi(decimals as i32);
    let amount_f = (notional_usd / price_usd / (1.0 + slip) * scale).floor();
    let amount_tokens = u64_from_f64_floor(amount_f)?;
    if amount_tokens == 0 {
        return None;
    }
    let lamports = notional_usd / sol_usd * LAMPORTS_PER_SOL;
    let base = u64_from_f64_floor(lamports)?;
    // Fee-inclusive cost with per-leg ceil, then the slippage guard.
    let with_fee = fee_inclusive_cost(base, fee_bps_per_side)?;
    // One guard everywhere.
    let max_sol_cost = apply_slippage_ceiling(with_fee, slippage_bps)?;
    Some(LiveBuyPlan {
        amount_tokens,
        max_sol_cost,
    })
}

/// Sell floor: modeled uncapped proceeds minus slippage. Fee tier is
/// required; `decimals` is explicit like in [`plan_buy`].
/// `None` on bad inputs, unpriceable proceeds, and zero floor (zero
/// authorizes any price).
pub fn plan_sell_floor(
    amount_tokens: u64,
    sol_usd: f64,
    slippage_bps: u64,
    fee_bps_per_side: f64,
    decimals: u8,
    mark: &simulator::Mark,
) -> Option<u64> {
    if amount_tokens == 0 || !mark.price.is_finite() || mark.price <= 0.0 {
        return None;
    }
    if !sol_usd.is_finite() || sol_usd <= 0.0 {
        return None;
    }
    if slippage_bps >= BPS_DENOMINATOR {
        return None;
    }
    if !fee_bps_per_side.is_finite() || fee_bps_per_side < 0.0 {
        return None;
    }
    if decimals > 18 {
        return None;
    }
    // Exact state when the exit tick carried reserves: fee-deducted net
    // for the actual size, then the floor under slippage — the same sell
    // math the paper book books and the executor re-quotes at dispatch.
    // Unpriceable (drained curve) fails closed.
    if let Some(curve) = mark.curve {
        let net = curve.sell_net_lamports(amount_tokens, fee_bps_per_side)?;
        if net == 0 {
            return None;
        }
        let floor = (u128::from(net) * (u128::from(BPS_DENOMINATOR) - u128::from(slippage_bps))
            / u128::from(BPS_DENOMINATOR)) as u64;
        return if floor == 0 { None } else { Some(floor) };
    }
    let scale = 10f64.powi(decimals as i32);
    // Proxy basis (tests, synthetic prices): floor from uncapped modeled
    // proceeds. `None` when the proceeds are unpriceable (bad/unknown
    // liquidity) — no capped fiction.
    let gross_notional = amount_tokens as f64 / scale * mark.price;
    let slip = FillModel::default()
        .with_venue_fee_bps(fee_bps_per_side)
        .try_slip_fraction_for_side(Side::Short, gross_notional, mark.liquidity_usd)?;
    let lamports = gross_notional * (1.0 - slip).max(0.0) / sol_usd * LAMPORTS_PER_SOL;
    // Same `>=` edge as `plan_buy` (cast saturates).
    if !lamports.is_finite() || lamports <= 0.0 || lamports >= u64::MAX as f64 {
        return None;
    }
    // Exact mul-div floor in u128. The `as u64` below is exact, not
    // saturating-by-accident: the guard above already rejected NaN,
    // non-positive, and everything >= 2^64 (unrepresentable in f64
    // anyway), so every surviving value converts losslessly.
    let base = lamports.floor() as u64;
    debug_assert!((base as f64) <= lamports);
    if base == 0 {
        return None;
    }
    let floor = (base as u128 * (BPS_DENOMINATOR as u128 - slippage_bps as u128)
        / BPS_DENOMINATOR as u128) as u64;
    if floor == 0 {
        return None;
    }
    Some(floor)
}

/// Jito-routed trader over a [`SwapExecutor`].
#[derive(Clone)]
pub struct LiveTrader {
    executor: SwapExecutor,
    slippage_bps: u64,
    tip_lamports: u64,
    /// Priority price for cost math.
    priority_fee_micro_lamports: u64,
    /// Compute limit for cost math.
    compute_unit_limit: u32,
    /// WAL path (`None` disables IO).
    wal_path: Option<PathBuf>,
    /// WAL write failures latch entries off: the claim of record is
    /// broken until an operator restart. Shared (clones agree).
    wal_degraded: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Unconfirmed buys, re-polled until settled or out of tries.
    pending: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, PendingBuy>>>,
    /// Recent mark prices per mint for volatility-adaptive slippage.
    /// Fed from live dispatch prints (entry/exit); tick-level feed is
    /// future work, so thin histories simply yield the base slippage.
    marks: MarkTrail,
    /// Volatility window, secs (`0` = adaptive off, base slippage only).
    vol_window_secs: u64,
    /// Cap on the volatility add-on, bps (`0` = adaptive off).
    vol_extra_cap_bps: u64,
    /// Last sell bundle per mint (resend guard: never compete with an
    /// inflight prior bundle for the same bag). Entries are resolved by
    /// [`LiveTrader::prior_sell_state`] before every release; only the
    /// [`SELL_GUARD_TTL_SECS`] bound releases without proof, so a lying
    /// status API cannot hold resends forever.
    last_sell_bundle:
        std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, SellGuard>>>,
    /// One recheck round at a time.
    recheck_busy: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// One dispatched sell awaiting resolution (resend-guard entry).
#[derive(Debug, Clone)]
struct SellGuard {
    /// Engine bundle UUID.
    bundle_id: String,
    /// Tx signature for reconciliation (recent + history). `None` only
    /// for legacy WAL traces that predate the field: the engine word
    /// governs those.
    signature: Option<String>,
    /// Dispatch blockhash (base58) for the expiry proof. `None` on
    /// legacy traces, same caveat as `signature`.
    blockhash: Option<String>,
    /// Dispatch stamp for the TTL release.
    noted_at: u64,
}

/// Prior-sell resolution for the resend guard (policy vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PriorSell {
    /// Unresolved: the prior may still land, hold resends.
    Hold,
    /// Landed on-chain: release, and surface the possibly stale ledger.
    Landed,
    /// Provably no-fill: release silently.
    Dead,
}

/// Buy awaiting confirmation. Never a fill until the chain says so.
#[derive(Debug, Clone)]
struct PendingBuy {
    position: LivePosition,
    signature: String,
    bundle_id: String,
    /// Dispatch blockhash (base58): proves expiry when the signature
    /// stays unseen. `None` for pending buys replayed from older WAL
    /// lines (they park manual instead of expiring).
    blockhash: Option<String>,
    attempts: u32,
}

/// One recheck round result per mint.
#[derive(Debug, Clone)]
pub struct PendingRecheck {
    /// Expected position (valid only on `Confirmed`).
    pub position: LivePosition,
    /// Engine bundle UUID.
    pub bundle_id: String,
    /// Tx signature (lookup on GiveUp).
    pub signature: String,
    pub outcome: RecheckOutcome,
}

/// Recheck verdict for one pending buy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecheckOutcome {
    /// Landed: track it and automate exits.
    Confirmed,
    /// Provably dead: nothing held, drop it. `FailedOnChain` burned
    /// base + priority on-chain (the tip rolled back).
    Dead(DeadReason),
    /// Still unknown: keep for next round.
    StillUnknown,
    /// Out of tries: drop and escalate.
    GiveUp,
}


impl LiveTrader {
    /// Wrap an executor with sizing policy.
    pub fn new(executor: SwapExecutor, slippage_bps: u64, tip_lamports: u64) -> Self {
        Self {
            executor,
            slippage_bps,
            tip_lamports,
            priority_fee_micro_lamports: 0,
            compute_unit_limit: 0,
            wal_path: None,
            wal_degraded: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            marks: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            vol_window_secs: 0,
            vol_extra_cap_bps: 0,
            pending: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            last_sell_bundle: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            recheck_busy: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Enable the crash-recovery WAL at `path`.
    pub fn with_wal(mut self, path: PathBuf) -> Self {
        self.wal_path = Some(path);
        self
    }

    /// Compute knobs for cost math (mirrors executor).
    pub fn with_dispatch_fees(
        mut self,
        compute_unit_limit: u32,
        priority_fee_micro_lamports: u64,
    ) -> Self {
        self.compute_unit_limit = compute_unit_limit;
        self.priority_fee_micro_lamports = priority_fee_micro_lamports;
        self
    }

    /// Volatility-adaptive slippage: `window_secs` of mark history per
    /// mint, with the add-on capped at `extra_cap_bps`. Zero/zero (the
    /// default) is bit-identical to static `slippage_bps`.
    pub fn with_adaptive_slippage(mut self, window_secs: u64, extra_cap_bps: u64) -> Self {
        self.vol_window_secs = window_secs;
        self.vol_extra_cap_bps = extra_cap_bps;
        self
    }

    /// Record a live print for the volatility window. Degenerate
    /// prices are ignored (never poison the range).
    pub fn note_mark(&self, mint: &str, price_usd: f64, now_secs: u64) {
        if !price_usd.is_finite() || price_usd <= 0.0 || self.vol_window_secs == 0 {
            return;
        }
        let window = self.vol_window_secs;
        let cutoff = now_secs.saturating_sub(window);
        match self.marks.lock() {
            Ok(mut guard) => {
                let q = guard.entry(mint.to_string()).or_default();
                q.push_back((now_secs, price_usd));
                while q.front().is_some_and(|(ts, _)| *ts < cutoff) {
                    q.pop_front();
                }
                // Bounded memory even when the clock stalls.
                while q.len() > 1024 {
                    q.pop_front();
                }
            }
            Err(_) => {
                tracing::warn!("marks-tracker lock poisoned during note_mark; skipping");
            }
        }
    }

    /// Slippage for this dispatch: base plus the in-window range as
    /// bps, capped. Thin or calm histories yield exactly the base, so
    /// static behavior is the default, not a special case.
    pub fn effective_slippage_bps(&self, mint: &str, now_secs: u64) -> u64 {
        let base = self.slippage_bps;
        if self.vol_window_secs == 0 || self.vol_extra_cap_bps == 0 {
            return base;
        }
        let window = self.vol_window_secs;
        let cutoff = now_secs.saturating_sub(window);
        let range_bps = match self.marks.lock() {
            Ok(mut guard) => {
                let Some(q) = guard.get_mut(mint) else {
                    return base;
                };
                while q.front().is_some_and(|(ts, _)| *ts < cutoff) {
                    q.pop_front();
                }
                let (mut lo, mut hi) = (f64::INFINITY, 0.0f64);
                for (_, p) in q.iter() {
                    lo = lo.min(*p);
                    hi = hi.max(*p);
                }
                if !(hi > lo && lo > 0.0 && hi.is_finite()) {
                    return base;
                }
                ((hi - lo) / lo * BPS_DENOMINATOR as f64).max(0.0)
            }
            Err(_) => {
                tracing::warn!("marks-tracker lock poisoned during effective_slippage; using base");
                return base;
            }
        };
        if !range_bps.is_finite() {
            return base;
        }
        base.saturating_add((range_bps.floor() as u64).min(self.vol_extra_cap_bps))
            .min(BPS_DENOMINATOR.saturating_sub(1))
    }

    /// Per-dispatch overhead: tip plus fees. Excludes first-buy ATA rent.
    pub fn dispatch_cost_lamports(&self) -> u64 {
        self.dispatch_cost_for_tip(self.tip_lamports)
    }

    /// Overhead at an explicit tip (escalated retries debit actuals, not
    /// the base rate).
    pub fn dispatch_cost_for_tip(&self, tip_lamports: u64) -> u64 {
        let prio = priority_fee_lamports(
            self.compute_unit_limit,
            self.priority_fee_micro_lamports,
        );
        tip_lamports
            .saturating_add(prio)
            .saturating_add(BASE_TX_FEE_LAMPORTS)
    }

    /// What a dispatch that landed and errored actually charged: base
    /// plus priority. The tip transfer executes only on success, so a
    /// failed tx rolls it back and this is the honest burn.
    pub fn failed_dispatch_cost_lamports(&self) -> u64 {
        self.dispatch_cost_for_tip(0)
    }

    /// First-buy cost: dispatch plus one ATA rent.
    pub fn first_touch_cost_lamports(&self) -> u64 {
        self.dispatch_cost_lamports()
            .saturating_add(ATA_RENT_LAMPORTS)
    }

    /// Run `f` under the pending lock (one poison policy).
    fn with_pending<R>(        &self,
        op: &'static str,
        fallback: R,
        f: impl FnOnce(&mut std::collections::HashMap<String, PendingBuy>) -> R,
    ) -> R {
        match self.pending.lock() {
            Ok(mut guard) => f(&mut guard),
            Err(_) => {
                tracing::warn!("pending-tracker lock poisoned during {op}; using fallback");
                fallback
            }
        }
    }

    /// Remember a sell bundle for the resend guard, and trace it in
    /// the WAL so a crash between dispatch and confirm does not blind
    /// the next boot's guard (replay seeds recent traces back). The
    /// signature is kept for reconciliation before any release.
    fn note_sell_bundle(&self, mint: &str, receipt: &JitoReceipt) {
        if let Ok(mut guard) = self.last_sell_bundle.lock() {
            guard.insert(
                mint.to_string(),
                SellGuard {
                    bundle_id: receipt.bundle_id.clone(),
                    signature: Some(receipt.signature.clone()),
                    blockhash: Some(receipt.blockhash.to_string()),
                    noted_at: now_secs(),
                },
            );
        } else {
            tracing::warn!(
                "sell-bundle lock poisoned during note_sell_bundle; resend guard degraded"
            );
        }
        self.write_wal_sell_pending(mint, receipt);
    }

    /// Forget a sell bundle after terminal confirm (landed or dead),
    /// retiring its WAL trace with it.
    fn clear_sell_bundle(&self, mint: &str) {
        if let Ok(mut guard) = self.last_sell_bundle.lock() {
            if let Some(entry) = guard.remove(mint) {
                self.write_wal_settled(&entry.bundle_id);
            }
        } else {
            tracing::warn!(
                "sell-bundle lock poisoned during clear_sell_bundle; resend guard degraded"
            );
        }
    }

    /// True when a guard entry outlived its TTL (pure, unit-pinned).
    fn sell_guard_expired(noted_at_secs: u64, now_secs: u64) -> bool {
        now_secs.saturating_sub(noted_at_secs) >= SELL_GUARD_TTL_SECS
    }

    /// Resolve a prior sell dispatch into guard policy: the shared
    /// resolver decides (chain beats engine; only a dead blockhash
    /// closes it as expired; unverifiable never releases). A legacy
    /// trace without a signature gets the engine word only — those
    /// lines age out of the WAL.
    async fn prior_sell_state(&self, entry: &SellGuard) -> PriorSell {
        match entry.signature.as_deref() {
            Some(signature) => match self
                .executor
                .resolve_dispatch(
                    &entry.bundle_id,
                    signature,
                    entry.blockhash.as_deref(),
                    SELL_GUARD_RESOLVE_TIMEOUT,
                )
                .await
            {
                DispatchVerdict::Unknown(_) => PriorSell::Hold,
                DispatchVerdict::Landed => PriorSell::Landed,
                DispatchVerdict::Dead(_) => PriorSell::Dead,
            },
            None => match self.executor.bundle_status(&entry.bundle_id).await {
                BundleStatus::Pending | BundleStatus::Unknown => PriorSell::Hold,
                BundleStatus::Landed => PriorSell::Landed,
                BundleStatus::Failed | BundleStatus::Invalid => PriorSell::Dead,
            },
        }
    }

    /// Seed the resend guard from WAL-replayed sell traces. A landed
    /// prior is surfaced loudly and NOT seeded (the bag may already be
    /// sold: a hold would only strand the exit); an unresolved one
    /// holds until it resolves. The TTL runs from the dispatch stamp,
    /// not from boot. Returns the kept count.
    pub async fn seed_sell_guard(&self, items: Vec<wal::WalSellPending>) -> usize {
        let mut kept = 0usize;
        for item in items {
            let entry = SellGuard {
                bundle_id: item.bundle_id.clone(),
                signature: item.signature.clone(),
                blockhash: item.blockhash.clone(),
                noted_at: item.ts,
            };
            let (mint, bundle_id) = (item.mint.clone(), item.bundle_id.clone());
            match self.prior_sell_state(&entry).await {
                PriorSell::Hold => {
                    if let Ok(mut guard) = self.last_sell_bundle.lock() {
                        guard.insert(mint.clone(), entry);
                        kept += 1;
                    }
                    tracing::warn!(%mint, %bundle_id, "WAL replay: prior sell unresolved; resends hold until it resolves");
                }
                PriorSell::Landed => {
                    tracing::warn!(%mint, %bundle_id, "WAL replay: prior sell ALREADY LANDED on-chain; not seeding a hold — ledger may be stale, inspect")
                }
                PriorSell::Dead => {}
            }
        }
        kept
    }

    /// True when the last sell bundle for `mint` may still land:
    /// resends must wait under the same cooldown, never compete with it.
    /// [`prior_sell_state`] carries the evidence rules; the TTL is the
    /// only evidence-free release, and it is loud.
    pub(crate) async fn prior_sell_pending(&self, mint: &str) -> bool {
        let entry = match self.last_sell_bundle.lock() {
            Ok(guard) => guard.get(mint).cloned(),
            Err(_) => None,
        };
        let Some(entry) = entry else { return false };
        if Self::sell_guard_expired(entry.noted_at, now_secs()) {
            if let Ok(mut guard) = self.last_sell_bundle.lock() {
                guard.remove(mint);
            }
            tracing::warn!(%mint, bundle = %entry.bundle_id, signature = ?entry.signature,
                "resend guard expired (TTL); prior sell unresolved for an hour — releasing resend, inspect the bundle on explorer");
            return false;
        }
        match self.prior_sell_state(&entry).await {
            PriorSell::Hold => true,
            PriorSell::Landed => {
                tracing::warn!(%mint, bundle = %entry.bundle_id, signature = ?entry.signature,
                    "prior sell LANDED on-chain; releasing resend — ledger may be stale, MANUAL REVIEW if the bag no longer exists");
                false
            }
            PriorSell::Dead => false,
        }
    }

    /// Raw ATA balance for `mint` in base units (boot reconciliation).
    /// `Ok(None)` = no ATA (nothing held); `Err` = unreadable (fail
    /// closed: never read as zero).
    pub async fn observe_token_balance(&self, mint: &Pubkey) -> Result<Option<u64>, ExecutorError> {
        self.executor.observe_token_balance(mint).await
    }

    /// True while a buy for `mint` awaits confirmation. Late-confirm
    /// rechecks ride that mint's ticks, so the watch must not drop.
    pub fn has_pending_for(&self, mint: &str) -> bool {
        self.with_pending("has_pending_for", false, |guard| guard.contains_key(mint))
    }

    /// True when a buy awaits confirmation.
    pub fn has_pending(&self) -> bool {
        self.with_pending("has_pending", true, |guard| !guard.is_empty())
    }

    /// Pending mints (cloned).
    pub fn pending_mints(&self) -> Vec<String> {
        self.with_pending("pending_mints", Vec::new(), |guard| {
            guard.keys().cloned().collect()
        })
    }

    /// Re-poll pending buys once. Unknown stays until budget, then GiveUp.
    pub async fn recheck_pending(&self) -> Vec<PendingRecheck> {
        if self
            .recheck_busy
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            // Skips are routine; debug level.
            tracing::debug!("pending recheck already in flight; skipping round");
            return Vec::new();
        }
        // Reset on every exit: a stuck flag silences rechecks.
        struct ResetOnDrop {
            busy: std::sync::Arc<std::sync::atomic::AtomicBool>,
        }
        impl Drop for ResetOnDrop {
            fn drop(&mut self) {
                self.busy.store(false, std::sync::atomic::Ordering::Release);
            }
        }
        let _reset = ResetOnDrop {
            busy: std::sync::Arc::clone(&self.recheck_busy),
        };
        let snapshot: Vec<PendingBuy> =
            self.with_pending("recheck-snapshot", Vec::new(), |guard| {
                guard.values().cloned().collect()
            });
        let results: Vec<(PendingBuy, RecheckOutcome)> =
            futures_util::stream::iter(snapshot.into_iter().map(|entry| async {
                // One resolution per round: chain poll, engine word,
                // signature reconcile, expiry — never "timeout = failed".
                let outcome = match self
                    .executor
                    .resolve_dispatch(
                        &entry.bundle_id,
                        &entry.signature,
                        entry.blockhash.as_deref(),
                        LIVE_CONFIRM_TIMEOUT,
                    )
                    .await
                {
                    DispatchVerdict::Landed => RecheckOutcome::Confirmed,
                    DispatchVerdict::Dead(reason) => {
                        tracing::warn!(mint = %entry.position.mint, bundle = %entry.bundle_id, %reason,
                            "pending LIVE buy provably dead; no fill");
                        RecheckOutcome::Dead(reason)
                    }
                    DispatchVerdict::Unknown(why) => {
                        let attempts = entry.attempts.saturating_add(1);
                        if attempts >= PENDING_MAX_ATTEMPTS {
                            // Still unresolved after the budget: park
                            // (may hold), never drop a maybe-fill.
                            tracing::warn!(mint = %entry.position.mint, bundle = %entry.bundle_id, why,
                                "pending LIVE buy unresolved past the retry budget");
                            RecheckOutcome::GiveUp
                        } else {
                            let mint = entry.position.mint.to_string();
                            self.with_pending("recheck-bump", (), |guard| {
                                if let Some(stored) = guard.get_mut(mint.as_str()) {
                                    stored.attempts = attempts;
                                }
                            });
                            RecheckOutcome::StillUnknown
                        }
                    }
                };
                (entry, outcome)
            }))
            .buffered(RECHECK_FANOUT)
            .collect()
            .await;
        let mut out = Vec::with_capacity(results.len());
        for (entry, outcome) in results {
            match outcome {
                RecheckOutcome::StillUnknown => {}
                // Terminal verdicts retire the WAL line.
                _ => {
                    self.write_wal_settled(&entry.bundle_id);
                    let mint = entry.position.mint.to_string();
                    self.with_pending("recheck-settle", (), |guard| {
                        guard.remove(mint.as_str());
                    });
                }
            }
            out.push(PendingRecheck {
                position: entry.position,
                bundle_id: entry.bundle_id,
                signature: entry.signature,
                outcome,
            });
        }
        out
    }

    /// Build a tracked position (one constructor).
    fn pending_position(
        mint: Pubkey,
        curve: Pubkey,
        assoc_curve: Pubkey,
        amount_tokens: u64,
        creator: Option<Pubkey>,
    ) -> LivePosition {
        LivePosition {
            mint,
            curve,
            assoc_curve,
            amount_tokens,
            creator,
        }
    }

    /// Observe the landed buy fill for entry-gap calibration. Returns
    /// the fill priced against the paper quote, or `None` when
    /// unobservable (fetch/parse failure — calibration skips, custody
    /// unaffected). Single bounded fetch; no retry.
    async fn observe_entry_fill(
        &self,
        signature: &str,
        mint_str: &str,
        decimals: u8,
        tip_lamports: u64,
        sol_usd: f64,
    ) -> Option<BuyFill> {
        let fill = match self
            .executor
            .observe_buy_fill(signature, mint_str, tip_lamports)
            .await
        {
            Some(fill) => fill,
            None => {
                tracing::debug!(%mint_str, "buy fill unobservable; entry gap skipped");
                return None;
            }
        };
        let fill_price_usd = buy_fill_price_usd(
            fill.swap_sol_lamports,
            fill.tokens_base_units,
            decimals,
            sol_usd,
        )?;
        let tokens = fill.tokens_base_units as f64 / 10f64.powi(decimals as i32);
        if !tokens.is_finite() || tokens <= 0.0 {
            return None;
        }
        Some(BuyFill {
            fill_price_usd,
            tokens,
        })
    }

    /// Buy `notional_usd` at `price_usd` via Jito. The fill observation
    /// rides the return triple (`None` when unobservable); late-confirm
    /// pending buys skip it (no price context travels with them).
    pub async fn buy(
        &self,
        req: &LiveBuyParams,
    ) -> Result<(LivePosition, String, Option<BuyFill>), LiveError> {
        let (mint, curve, assoc_curve, notional_usd, sol_usd) = (
            req.mint,
            req.curve,
            req.assoc_curve,
            req.notional_usd,
            req.sol_usd,
        );
        let price_usd = req.mark.price;
        let creator = req.creator;
        // String-boundary uses (marks, pending map, logs) derive once
        // from the validated key; dispatch uses the `Pubkey` directly.
        let mint_str = mint.to_string();
        let plan = plan_buy(
            notional_usd,
            sol_usd,
            self.effective_slippage_bps(&mint_str, now_secs()),
            req.venue_fee_bps.unwrap_or(VENUE_FEE_BPS as f64),
            req.decimals,
            &req.mark,
        )
        .ok_or_else(|| LiveError::BadOrder(format!("cannot size buy of {mint} at {price_usd}")))?;
        self.note_mark(&mint_str, price_usd, now_secs());
        // Bound once: the order and the gap math must agree on the tip
        // paid (a second oracle read could drift between the two).
        let tip_lamports = self.effective_tip(0).await;
        let receipt = match tokio::time::timeout(
            LIVE_DISPATCH_TIMEOUT,
            self.executor.buy_jito(&BuyOrder {
                mint,
                bonding_curve: curve,
                associated_bonding_curve: assoc_curve,
                amount_tokens: plan.amount_tokens,
                max_sol_cost: plan.max_sol_cost,
                tip_lamports,
                creator,
                slippage_bps: self.effective_slippage_bps(&mint_str, now_secs()),
            }),
        )
        .await
        {
            Ok(dispatch) => dispatch?,
            Err(_) => {
                // No signature: fill unknown, park for review.
                let position = Self::pending_position(
                    mint,
                    curve,
                    assoc_curve,
                    plan.amount_tokens,
                    creator,
                );
                return Err(LiveError::UnknownDispatch {
                    position: Box::new(position),
                    reason: format!(
                        "buy dispatch of {mint} timed out after {:?}; fill unknown, MANUAL REVIEW REQUIRED",
                        LIVE_DISPATCH_TIMEOUT
                    ),
                });
            }
        };
        tracing::info!(%mint, bundle = %receipt.bundle_id, amount = plan.amount_tokens, "LIVE buy dispatched");
        // Receipt exists from here; confirm removes it.
        let intended = Self::pending_position(
            mint,
            curve,
            assoc_curve,
            plan.amount_tokens,
            creator,
        );
        Self::track_pending(&self.pending, &mint_str, intended.clone(), &receipt);
        self.write_wal_pending(&intended, &receipt);
        let dispatch_blockhash = receipt.blockhash.to_string();
        match self
            .executor
            .resolve_dispatch(
                &receipt.bundle_id,
                &receipt.signature,
                Some(dispatch_blockhash.as_str()),
                LIVE_CONFIRM_TIMEOUT,
            )
            .await
        {
            DispatchVerdict::Landed => {
                Self::untrack_pending(&self.pending, &mint_str);
                let entry_fill = self
                    .observe_entry_fill(
                        &receipt.signature,
                        &mint_str,
                        req.decimals,
                        tip_lamports,
                        sol_usd,
                    )
                    .await;
                Ok((
                    Self::pending_position(
                        mint,
                        curve,
                        assoc_curve,
                        plan.amount_tokens,
                        creator,
                    ),
                    receipt.bundle_id,
                    entry_fill,
                ))
            }
            DispatchVerdict::Dead(reason) => {
                Self::untrack_pending(&self.pending, &mint_str);
                // Proven no-fill: retire the line so boot skips it.
                self.write_wal_settled(&receipt.bundle_id);
                Err(LiveError::Dead {
                    reason,
                    context: format!("buy bundle {}; no fill", receipt.bundle_id),
                })
            }
            DispatchVerdict::Unknown(why) => {
                // May land late: keep for recheck, never resend.
                Err(LiveError::Unconfirmed(format!(
                    "buy bundle {} unconfirmed after {:?} ({why}); tracked as pending, rechecking",
                    receipt.bundle_id, LIVE_CONFIRM_TIMEOUT
                )))
            }
        }
    }

    /// Record an unconfirmed buy. First receipt wins per mint.
    fn track_pending(
        pending: &std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, PendingBuy>>>,
        mint: &str,
        position: LivePosition,
        receipt: &JitoReceipt,
    ) {
        match pending.lock() {
            Ok(mut guard) => {
                if let Some(existing) = guard.get(mint) {
                    tracing::warn!(mint = %mint, old_bundle = %existing.bundle_id, new_bundle = %receipt.bundle_id,
                        "second pending buy for the same mint; keeping the first, MANUAL REVIEW REQUIRED for the second");
                }
                guard.entry(mint.to_string()).or_insert(PendingBuy {
                    position,
                    signature: receipt.signature.clone(),
                    bundle_id: receipt.bundle_id.clone(),
                    blockhash: Some(receipt.blockhash.to_string()),
                    attempts: 1,
                });
            }
            // Poisoned lock would strand funds: escalate loudly.
            Err(_) => tracing::warn!(mint = %mint, bundle = %receipt.bundle_id,
                "pending-tracker lock poisoned; buy fill untracked, MANUAL REVIEW REQUIRED"),
        }
    }

    /// Remove a settled entry. Timeouts stay for recheck.
    fn untrack_pending(
        pending: &std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, PendingBuy>>>,
        mint: &str,
    ) {
        match pending.lock() {
            Ok(mut guard) => {
                guard.remove(mint);
            }
            Err(_) => tracing::warn!(mint = %mint,
                "pending-tracker lock poisoned; settled entry not removed"),
        }
    }

    /// Append one line (synced). A failure cannot undo the dispatch
    /// that already happened, but it latches entries off: no further
    /// live risk may open while custody cannot be recorded.
    fn wal_append(&self, line: String) {
        let Some(path) = self.wal_path.as_ref() else {
            return;
        };
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(parent);
        }
        use std::io::Write as _;
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(path) {
            Ok(mut f) => {
                if writeln!(f, "{line}").is_err() || f.sync_all().is_err() {
                    self.mark_wal_degraded("append/fsync failed");
                }
            }
            Err(e) => self.mark_wal_degraded(&format!("open failed: {e}")),
        }
    }

    /// Latch a WAL write failure. The current dispatch stands (it is
    /// already sent and tracked in memory); from here no new entry may
    /// open until an operator restart with a working WAL.
    fn mark_wal_degraded(&self, why: &str) {
        use std::sync::atomic::Ordering;
        if self.wal_degraded.swap(true, Ordering::Relaxed) {
            tracing::error!(reason = why, "live WAL write still failing");
        } else {
            tracing::error!(
                reason = why,
                "live WAL WRITE FAILED: claim of record broken; new entries halted until restart (exits and `sell <mint>` keep running)"
            );
        }
    }

    /// True once a WAL write failed this run (see `mark_wal_degraded`).
    pub fn wal_degraded(&self) -> bool {
        self.wal_degraded.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Append one typed line (synced). No-op without a WAL path; a
    /// serialize failure would be an engine bug: latched, not panicked.
    fn wal_record(&self, line: &wal::WalLine) {
        if self.wal_path.is_none() {
            return;
        }
        match serde_json::to_string(line) {
            Ok(json) => self.wal_append(json),
            Err(e) => self.mark_wal_degraded(&format!("serialize failed: {e}")),
        }
    }

    /// Record a dispatch BEFORE confirming.
    fn write_wal_pending(&self, position: &LivePosition, receipt: &JitoReceipt) {
        self.wal_record(&wal::WalLine::Pending {
            mint: position.mint.to_string(),
            curve: position.curve.to_string(),
            assoc: position.assoc_curve.to_string(),
            amount: position.amount_tokens,
            creator: position.creator.as_ref().map(|c| c.to_string()),
            sig: receipt.signature.clone(),
            bundle: receipt.bundle_id.clone(),
            blockhash: Some(receipt.blockhash.to_string()),
            ts: Some(now_secs()),
        });
    }

    /// Record a sell dispatch BEFORE confirming (crash window
    /// coverage: replay seeds the resend guard from unsettled lines).
    fn write_wal_sell_pending(&self, mint: &str, receipt: &JitoReceipt) {
        self.wal_record(&wal::WalLine::SellPending {
            mint: mint.to_string(),
            bundle: receipt.bundle_id.clone(),
            sig: Some(receipt.signature.clone()),
            blockhash: Some(receipt.blockhash.to_string()),
            ts: now_secs(),
        });
    }

    /// Record a dead bundle so boot skips it.
    fn write_wal_settled(&self, bundle_id: &str) {
        self.wal_record(&wal::WalLine::Settled {
            bundle: bundle_id.to_string(),
            ts: Some(now_secs()),
        });
    }

    /// Record a fill and retire pending. Call AFTER tracking it.
    pub fn acknowledge_buy(&self, position: &LivePosition, bundle_id: &str) {
        self.wal_record(&wal::WalLine::Position {
            mint: position.mint.to_string(),
            curve: position.curve.to_string(),
            assoc: position.assoc_curve.to_string(),
            amount: position.amount_tokens,
            creator: position.creator.as_ref().map(|c| c.to_string()),
            bundle: Some(bundle_id.to_string()),
            ts: Some(now_secs()),
        });
        self.write_wal_settled(bundle_id);
    }

    /// Record a human-exit bag. `note` is a lookup id (bundle,
    /// signature, or cause).
    pub fn record_manual(&self, position: &LivePosition, note: &str) {
        self.wal_record(&wal::WalLine::Manual {
            mint: position.mint.to_string(),
            curve: position.curve.to_string(),
            assoc: position.assoc_curve.to_string(),
            amount: position.amount_tokens,
            creator: position.creator.as_ref().map(|c| c.to_string()),
            note: Some(note.to_string()),
            ts: Some(now_secs()),
        });
    }

    /// Retire a manual bag taken for exit. Call AFTER taking: a crash
    /// between take and sell-settle must not replay the bag as a ghost.
    pub fn unrecord_manual(&self, position: &LivePosition) {
        self.wal_record(&wal::WalLine::TakeManual {
            mint: position.mint.to_string(),
            amount: position.amount_tokens,
            ts: Some(now_secs()),
        });
    }

    /// Record a confirmed sell.
    pub fn record_sell(&self, mint: &str, amount_tokens: u64, bundle_id: &str) {
        self.wal_record(&wal::WalLine::Sell {
            mint: mint.to_string(),
            amount: amount_tokens,
            bundle: Some(bundle_id.to_string()),
            ts: Some(now_secs()),
        });
    }

    /// Congestion pricing for heartbeat display: `(base, floor,
    /// worst)` where worst is 4x base lifted to the oracle floor —
    /// the per-dispatch figure paper legs never pay. Mirrors
    /// [`Self::effective_tip`] at max escalation.
    pub async fn congestion_snapshot(&self) -> (u64, u64, u64) {
        let base = self.tip_lamports;
        let esc = escalated_tip(base, u32::MAX);
        if base == 0 {
            return (0, 0, 0);
        }
        let floor = self.executor.tip_floor_lamports().await;
        (
            base,
            floor,
            esc.max(floor.min(base.saturating_mul(TIP_ESCALATION_MAX_MULT))),
        )
    }
    /// Congestion-aware tip for an attempt: escalated base lifted to
    /// the landed-tip floor, bounded at 4x base. Static tips underland
    /// in congestion and overpay in calm; the floor adapts both legs.
    /// Zero base stays zero (explicit tipless).
    pub async fn effective_tip(&self, failed_attempts: u32) -> u64 {
        let base = self.tip_lamports;
        let esc = escalated_tip(base, failed_attempts);
        if base == 0 {
            return esc;
        }
        let floor = self.executor.tip_floor_lamports().await;
        esc.max(floor.min(base.saturating_mul(TIP_ESCALATION_MAX_MULT)))
    }

    /// Import boot-replayed pendings into recheck.
    pub fn import_pending(&self, items: Vec<wal::WalPending>) -> usize {
        use std::collections::hash_map::Entry;
        // Pre-upgrade lines carry no blockhash: their expiry can never
        // be proven, so an unresolved one parks to manual instead of
        // dropping. Surface the count once, at the only moment it is
        // knowable.
        let legacy = items.iter().filter(|i| i.blockhash.is_none()).count();
        if legacy > 0 {
            tracing::warn!(
                legacy,
                "WAL replay: pending line(s) without blockhash (pre-upgrade); unresolved ones park to manual, never auto-drop"
            );
        }
        self.with_pending("wal-import", 0, |guard| {
            let mut n = 0usize;
            for item in items {
                let mint = item.position.mint.to_string();
                match guard.entry(mint) {
                    Entry::Vacant(slot) => {
                        n += 1;
                        slot.insert(PendingBuy {
                            position: item.position,
                            signature: item.signature,
                            bundle_id: item.bundle_id,
                            blockhash: item.blockhash,
                            attempts: 1,
                        });
                    }
                    // First wins, but never silently: a skipped pending
                    // is a buy the rechecker will never see (pre-restart
                    // dup-guard makes this rare, never impossible).
                    Entry::Occupied(_) => {
                        tracing::warn!(mint = %item.position.mint, bundle = %item.bundle_id,
                            "WAL replay: second pending for same mint skipped (first wins; inspect the file)");
                    }
                }
            }
            n
        })
    }

    /// Sell the full tracked amount via Jito.
    pub async fn sell(&self, req: &LiveSellParams<'_>) -> Result<String, LiveError> {
        let position = req.position;
        // String-boundary uses derive once; dispatch uses `Pubkey`s.
        let mint_str = position.mint.to_string();
        let slip = self.effective_slippage_bps(&mint_str, now_secs());
        let floor = plan_sell_floor(
            position.amount_tokens,
            req.sol_usd,
            slip,
            req.venue_fee_bps.unwrap_or(VENUE_FEE_BPS as f64),
            req.decimals,
            &req.mark,
        )
        .ok_or_else(|| LiveError::BadOrder(format!("cannot floor sell of {}", position.mint)))?;
        self.note_mark(&mint_str, req.mark.price, now_secs());
        let receipt = match tokio::time::timeout(
            LIVE_DISPATCH_TIMEOUT,
            self.executor.sell_jito(&SellOrder {
                mint: position.mint,
                bonding_curve: position.curve,
                associated_bonding_curve: position.assoc_curve,
                amount_tokens: position.amount_tokens,
                min_sol_output: floor,
                slippage_bps: slip,
                tip_lamports: req.tip_lamports,
                creator: position.creator,
            }),
        )
        .await
        {
            Ok(dispatch) => dispatch?,
            Err(_) => {
                return Err(LiveError::Unconfirmed(format!(
                    "sell dispatch of {} timed out after {:?}; still holding",
                    position.mint, LIVE_DISPATCH_TIMEOUT
                )));
            }
        };
        tracing::info!(mint = %position.mint, bundle = %receipt.bundle_id, "LIVE sell dispatched");
        self.note_sell_bundle(&mint_str, &receipt);
        self.confirm_pool_sell(&mint_str, &receipt, "sell").await
    }

    /// Confirm a pool-leg sell bundle. Shared by every venue (`label`
    /// names the leg in logs and errors). No WAL write here: the
    /// caller records the sell write-ahead of its custody subtract.
    async fn confirm_pool_sell(
        &self,
        mint: &str,
        receipt: &JitoReceipt,
        label: &str,
    ) -> Result<String, LiveError> {
        let dispatch_blockhash = receipt.blockhash.to_string();
        match self
            .executor
            .resolve_dispatch(
                &receipt.bundle_id,
                &receipt.signature,
                Some(dispatch_blockhash.as_str()),
                LIVE_CONFIRM_TIMEOUT,
            )
            .await
        {
            DispatchVerdict::Landed => {
                self.clear_sell_bundle(mint);
                Ok(receipt.bundle_id.clone())
            }
            DispatchVerdict::Dead(reason) => {
                // Terminally dead: never lands, so drop the id — the
                // resend guard must not pay a status query for it, nor
                // hold a resend for a bundle that cannot compete.
                self.clear_sell_bundle(mint);
                Err(LiveError::Dead {
                    reason,
                    context: format!("{label} bundle {}; still holding", receipt.bundle_id),
                })
            }
            DispatchVerdict::Unknown(why) => Err(LiveError::Unconfirmed(format!(
                "{label} bundle {} unconfirmed after {:?} ({why}); still holding",
                receipt.bundle_id, LIVE_CONFIRM_TIMEOUT
            ))),
        }
    }

    /// Raydium sell at `pool` via Jito. Floor prices from vault reserves.
    pub async fn sell_raydium(&self, req: &RaydiumSellParams<'_>) -> Result<String, LiveError> {
        let position = req.position;
        let pool = req.pool;
        let mint_str = position.mint.to_string();
        let receipt = match tokio::time::timeout(
            LIVE_DISPATCH_TIMEOUT,
            self.executor.sell_raydium_jito(&executor::SellRaydiumOrder {
                mint: position.mint,
                amm: pool.amm,
                base_vault: pool.base_vault,
                quote_vault: pool.quote_vault,
                amount_tokens: position.amount_tokens,
                slippage_bps: self.effective_slippage_bps(&mint_str, now_secs()),
                tip_lamports: req.tip_lamports,
            }),
        )
        .await
        {
            Ok(dispatch) => dispatch?,
            Err(_) => {
                return Err(LiveError::Unconfirmed(format!(
                    "raydium sell dispatch of {} timed out after {:?}; still holding",
                    position.mint, LIVE_DISPATCH_TIMEOUT
                )));
            }
        };
        tracing::info!(mint = %position.mint, bundle = %receipt.bundle_id, "LIVE raydium sell dispatched");
        self.note_sell_bundle(&mint_str, &receipt);
        self.confirm_pool_sell(&mint_str, &receipt, "raydium sell").await
    }

    /// PumpSwap sell at `pool` via Jito. Floor quotes from live
    /// reserves plus the fee schedule.
    pub async fn sell_pumpswap(&self, req: &PumpSwapSellParams<'_>) -> Result<String, LiveError> {
        let position = req.position;
        let pool = req.pool;
        let mint_str = position.mint.to_string();
        let receipt = match tokio::time::timeout(
            LIVE_DISPATCH_TIMEOUT,
            self.executor.sell_pumpswap_jito(&executor::SellPumpSwapOrder {
                mint: position.mint,
                pool: pool.amm,
                base_vault: pool.base_vault,
                quote_vault: pool.quote_vault,
                amount_tokens: position.amount_tokens,
                slippage_bps: self.effective_slippage_bps(&mint_str, now_secs()),
                tip_lamports: req.tip_lamports,
            }),
        )
        .await
        {
            Ok(dispatch) => dispatch?,
            Err(_) => {
                return Err(LiveError::Unconfirmed(format!(
                    "pumpswap sell dispatch of {} timed out after {:?}; still holding",
                    position.mint, LIVE_DISPATCH_TIMEOUT
                )));
            }
        };
        tracing::info!(mint = %position.mint, bundle = %receipt.bundle_id, "LIVE pumpswap sell dispatched");
        self.note_sell_bundle(&mint_str, &receipt);
        self.confirm_pool_sell(&mint_str, &receipt, "pumpswap sell").await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slippage_ceiling_covers_fee_inclusive_quotes() {
        assert_eq!(apply_slippage_ceiling(1_668_970, 100), Some(1_685_660));
        assert_eq!(apply_slippage_ceiling(10_001, 0), Some(10_001));
        assert_eq!(apply_slippage_ceiling(10_000, 1), Some(10_001));
        assert_eq!(apply_slippage_ceiling(0, 200), None);
        assert_eq!(apply_slippage_ceiling(1_000, 9_999), Some(2_000));
        assert_eq!(apply_slippage_ceiling(1_000, 10_000), None);
        assert_eq!(apply_slippage_ceiling(1_000, 99_999), None);
        assert_eq!(apply_slippage_ceiling(u64::MAX, 10_000), None);
        assert_eq!(apply_slippage_ceiling(u64::MAX, 0), Some(u64::MAX));
    }

    #[test]
    fn fee_inclusive_cost_covers_every_two_leg_split() {
        // Parity pin against `executor::quote::buy_cost_incl_fees`,
        // which ceils the protocol and creator legs separately:
        // `ceil(a) + ceil(b)` exceeds `ceil(a + b)` by at most one, so
        // single-ceil-plus-one must cover every split of the tier —
        // never underfund, never over-cover by more than a lamport.
        const D: u128 = BPS_DENOMINATOR as u128;
        let ceil_fee = |base: u64, bps: u64| -> u128 {
            u128::from(base).saturating_mul(u128::from(bps)).div_ceil(D)
        };
        for base in [
            0u64, 1, 2, 99, 100, 101, 999, 1024, 1_000_000, 123_456_789, 6_750_000_001,
            4_294_967_295, u64::MAX / 20_000,
        ] {
            for tier in [0u64, 1, 125, 250, 300, 1000, 5000] {
                for t1 in [0u64, 1, tier / 3, tier / 2, tier] {
                    if t1 > tier {
                        continue;
                    }
                    let t2 = tier - t1;
                    let Some(covered) = fee_inclusive_cost(base, tier as f64) else {
                        continue;
                    };
                    let onchain =
                        u128::from(base) + ceil_fee(base, t1) + ceil_fee(base, t2);
                    assert!(
                        u128::from(covered) >= onchain,
                        "underfunded: base={base} tier={tier} split={t1}+{t2}"
                    );
                    assert!(
                        u128::from(covered) - onchain <= 1,
                        "over-cover: base={base} tier={tier} split={t1}+{t2}"
                    );
                }
            }
        }
    }

    #[test]
    fn sell_guard_expires_on_ttl() {
        // A guard entry older than the TTL releases (loudly at the
        // call site): Jito bundles resolve in minutes, so an hour-old
        // hold is outage, not custody.
        assert!(!LiveTrader::sell_guard_expired(1_000, 1_000));
        assert!(!LiveTrader::sell_guard_expired(1_000, 1_000 + SELL_GUARD_TTL_SECS - 1));
        assert!(LiveTrader::sell_guard_expired(1_000, 1_000 + SELL_GUARD_TTL_SECS));
        assert!(LiveTrader::sell_guard_expired(1_000, 1_000 + SELL_GUARD_TTL_SECS + 3_600));
        // Clock skew never panics, never holds.
        assert!(!LiveTrader::sell_guard_expired(2_000, 1_000));
    }

    #[test]
    fn tip_escalates_bounded_by_attempts() {        assert_eq!(escalated_tip(100_000, 0), 100_000);
        assert_eq!(escalated_tip(100_000, 1), 200_000);
        assert_eq!(escalated_tip(100_000, 2), 300_000);
        assert_eq!(escalated_tip(100_000, 3), 400_000);
        // Capped: a stuck bag cannot burn the wallet.
        assert_eq!(escalated_tip(100_000, 4), 400_000);
        assert_eq!(escalated_tip(100_000, u32::MAX), 400_000);
        // Zero base stays zero; saturates instead of wrapping.
        assert_eq!(escalated_tip(0, 9), 0);
        assert_eq!(escalated_tip(u64::MAX, 9), u64::MAX);
    }

    #[tokio::test]
    async fn effective_tip_without_oracle_matches_escalation() {
        // No bundle client: floor is 0, configured tip rules.
        let trader = cost_trader(100_000, 0, 0);
        assert_eq!(trader.effective_tip(0).await, 100_000);
        assert_eq!(trader.effective_tip(2).await, 300_000);
        // Explicit tipless stays tipless (floor must not lift it).
        let bare = cost_trader(0, 0, 0);
        assert_eq!(bare.effective_tip(9).await, 0);
    }

    /// Proxy-basis test mark (spot + book liquidity, no curve state).
    fn mark(price_usd: f64, liquidity_usd: f64) -> simulator::Mark {
        simulator::Mark::new(price_usd, liquidity_usd)
    }

    /// Test mark carrying exact curve state.
    fn curve_mark(
        price_usd: f64,
        liquidity_usd: f64,
        curve: simulator::CurveReserves,
    ) -> simulator::Mark {
        simulator::Mark {
            price: price_usd,
            liquidity_usd,
            curve: Some(curve),
        }
    }

    #[test]
    fn buy_plan_math() {
        let plan =
            plan_buy(10.0, 150.0, 200, 125.0, 6, &mark(0.000_004_2, 30_000.0)).expect("plan");
        assert_eq!(plan.amount_tokens, 2_350_783_728_484);
        // Fee-inclusive cost 67_500_001 (single-ceil 67_500_000 plus the
        // double-ceil cover lamport), guarded at +200bps.
        assert_eq!(plan.max_sol_cost, 68_850_002);
    }

    #[test]
    fn buy_fill_price_math() {
        // 7_855_520 lamports for 2000 tokens (6dp) at SOL $150:
        // 0.00785552 * 150 / 2000 = 0.000589164 $/token.
        let price =
            buy_fill_price_usd(7_855_520, 2_000_000_000, 6, 150.0).expect("price");
        assert!((price - 0.000_589_164).abs() < 1e-12, "price={price}");
        // Degenerate inputs never divide by zero.
        assert_eq!(buy_fill_price_usd(0, 2_000_000_000, 6, 150.0), None);
        assert_eq!(buy_fill_price_usd(7_855_520, 0, 6, 150.0), None);
        assert_eq!(buy_fill_price_usd(7_855_520, 2_000_000_000, 6, 0.0), None);
        assert_eq!(buy_fill_price_usd(7_855_520, 2_000_000_000, 6, f64::NAN), None);
        assert_eq!(buy_fill_price_usd(7_855_520, 2_000_000_000, 19, 150.0), None);
    }

    #[test]
    fn buy_plan_custom_fee_sizes_and_funds_at_tier() {
        let std = plan_buy(10.0, 150.0, 200, 125.0, 6, &mark(0.000_004_2, 30_000.0)).expect("std");
        let cus =
            plan_buy(10.0, 150.0, 200, 345.0, 6, &mark(0.000_004_2, 30_000.0)).expect("custom");
        assert!(cus.amount_tokens < std.amount_tokens);
        assert!(cus.max_sol_cost > std.max_sol_cost);
        use simulator::{PaperPortfolio, Side};
        let mut paper = PaperPortfolio::new(1_000.0, 0.0);
        paper.set_venue_fee_for_mint("MINT", 345.0);
        let id = paper
            .open_position_params(&simulator::OpenPositionParams {
                mint: "MINT",
                side: Side::Long,
                mark: simulator::Mark::new(0.000_004_2, 30_000.0),
                notional_usd: 10.0,
                tp_percent: 75.0,
                sl_percent: -8.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 1_000,
            })
            .expect("open");
        let qty = paper.position(id).expect("pos").quantity;
        // Paper tracks whole tokens, live sizes base units: the two agree
        // to sub-token dust (< 1 token ≈ micro-USD at sniper prices).
        assert!((cus.amount_tokens as i128 - qty as i128 * 1_000_000).abs() < 1_000_000);
    }

    #[test]
    fn buy_plan_rejects_curve_exhaustion() {
        // Parity with `PaperPortfolio::open_position_params`
        // (`CurveExhausted`): a buy at/above liquidity is unpriceable,
        // never a capped fill.
        assert_eq!(
            plan_buy(30_000.0, 150.0, 200, 125.0, 6, &mark(0.000_004_2, 30_000.0)),
            None
        );
        assert_eq!(
            plan_buy(60_000.0, 150.0, 200, 125.0, 6, &mark(0.000_004_2, 30_000.0)),
            None
        );
        // Just below still prices.
        assert!(plan_buy(29_999.0, 150.0, 200, 125.0, 6, &mark(0.000_004_2, 30_000.0)).is_some());
        // Unknown liquidity is unpriceable too.
        assert_eq!(
            plan_buy(10.0, 150.0, 200, 125.0, 6, &mark(0.000_004_2, f64::NAN)),
            None
        );
        // ... and the paper leg agrees (CurveExhausted, not a fill).
        use simulator::{PaperPortfolio, PortfolioError, Side};
        let mut paper = PaperPortfolio::new(1_000_000.0, 0.0);
        let err = paper
            .open_position_params(&simulator::OpenPositionParams {
                mint: "MINT",
                side: Side::Long,
                mark: simulator::Mark::new(0.000_004_2, 30_000.0),
                notional_usd: 30_000.0,
                tp_percent: 75.0,
                sl_percent: -8.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 1_000,
            })
            .expect_err("exhausted");
        assert!(matches!(err, PortfolioError::CurveExhausted { .. }));
    }

    #[test]
    fn plan_buy_threads_decimals() {
        let six = plan_buy(10.0, 150.0, 200, 125.0, 6, &mark(0.000_004_2, 30_000.0)).expect("plan");
        let nine =
            plan_buy(10.0, 150.0, 200, 125.0, 9, &mark(0.000_004_2, 30_000.0)).expect("plan");
        assert_eq!(nine.amount_tokens / 1000, six.amount_tokens);
        assert_eq!(nine.max_sol_cost, six.max_sol_cost);
        assert_eq!(
            plan_buy(10.0, 150.0, 200, 125.0, 19, &mark(0.000_004_2, 30_000.0)),
            None
        );
    }

    #[test]
    fn fee_inclusive_cost_covers_split_ceil() {
        // Single-ceil plus one: covers the on-chain per-leg double ceil
        // (quote.rs:325-328) for every (proto, creator) split.
        assert_eq!(fee_inclusive_cost(66_666_666, 125.0), Some(67_500_001));
        assert_eq!(fee_inclusive_cost(1, 1.0), Some(3));
        assert_eq!(fee_inclusive_cost(10_000, 0.0), Some(10_001));
        assert_eq!(fee_inclusive_cost(1, f64::NAN), None);
        assert_eq!(fee_inclusive_cost(1, -1.0), None);
        assert_eq!(fee_inclusive_cost(u64::MAX, 10_000.0), None);
    }

    #[test]
    fn buy_plan_rejects_degenerate() {
        assert_eq!(
            plan_buy(0.0, 150.0, 200, 125.0, 6, &mark(1.0, 30_000.0)),
            None
        );
        assert_eq!(
            plan_buy(10.0, 150.0, 200, 125.0, 6, &mark(0.0, 30_000.0)),
            None
        );
        assert_eq!(
            plan_buy(10.0, 0.0, 200, 125.0, 6, &mark(1.0, 30_000.0)),
            None
        );
        assert_eq!(
            plan_buy(f64::NAN, 150.0, 200, 125.0, 6, &mark(1.0, 30_000.0)),
            None
        );
        assert_eq!(
            plan_buy(1e-12, 150.0, 200, 125.0, 6, &mark(1.0, 30_000.0)),
            None
        );
        assert_eq!(
            plan_buy(10.0, 150.0, 200, f64::NAN, 6, &mark(1.0, 30_000.0)),
            None
        );
        assert_eq!(
            plan_buy(10.0, 150.0, 200, -1.0, 6, &mark(1.0, 30_000.0)),
            None
        );
    }

    #[test]
    fn sell_floor_math() {
        let floor =
            plan_sell_floor(1_000_000, 150.0, 200, 125.0, 6, &mark(2.0, 30_000.0)).expect("floor");
        assert_eq!(floor, 12_902_461);
    }

    #[test]
    fn sell_floor_custom_fee_floors_lower() {
        let std =
            plan_sell_floor(1_000_000, 150.0, 200, 125.0, 6, &mark(2.0, 30_000.0)).expect("std");
        let cus =
            plan_sell_floor(1_000_000, 150.0, 200, 345.0, 6, &mark(2.0, 30_000.0)).expect("custom");
        assert!(cus < std, "custom {cus} vs std {std}");
    }

    #[test]
    fn u64_boundary_fails_closed() {
        assert_eq!(u64_from_f64_floor(2f64.powi(64)), None);
        assert_eq!(u64_from_f64_floor(f64::NAN), None);
        assert_eq!(u64_from_f64_floor(f64::INFINITY), None);
        assert_eq!(u64_from_f64_floor(0.0), None);
        assert_eq!(u64_from_f64_floor(-1.0), None);
        assert_eq!(u64_from_f64_floor(1e10), Some(10_000_000_000));
        assert_eq!(u64_from_f64_floor(1.9), Some(1));
    }

    #[test]
    fn buy_plan_rejects_overflow() {
        assert_eq!(
            plan_buy(1e20, 150.0, 200, 125.0, 6, &mark(1e-10, 30_000.0)),
            None
        );
        assert_eq!(
            plan_buy(1e13, 150.0, 200, 125.0, 6, &mark(1e6, 30_000.0)),
            None
        );
    }

    #[test]
    fn sell_floor_rejects_zero_floor() {
        assert_eq!(
            plan_sell_floor(1_000_000, 150.0, 10_000, 125.0, 6, &mark(2.0, 30_000.0)),
            None
        );
        assert_eq!(
            plan_sell_floor(1_000_000, 150.0, 10_001, 125.0, 6, &mark(2.0, 30_000.0)),
            None
        );
        assert_eq!(
            plan_sell_floor(1, 1e9, 200, 125.0, 6, &mark(1e-9, 30_000.0)),
            None
        );
    }

    #[test]
    fn sell_floor_slippage_floors_in_integers() {
        // Unknown liquidity is unpriceable: fail closed, no fee-only guess.
        assert_eq!(
            plan_sell_floor(1, 150.0, 0, 125.0, 6, &mark(0.2, f64::NAN)),
            None
        );
        assert_eq!(
            plan_sell_floor(1, 150.0, 0, 125.0, 6, &mark(0.2, -1.0)),
            None
        );
        // Exact one-token proceeds: the slippage haircut floors in integers.
        assert_eq!(
            plan_sell_floor(1_000_000, 150.0, 0, 0.0, 6, &mark(150.0, 0.0)),
            Some(1_000_000_000)
        );
        assert_eq!(
            plan_sell_floor(1_000_000, 150.0, 1, 0.0, 6, &mark(150.0, 0.0)),
            Some(999_900_000)
        );
        let floored = (10_001u64 as u128 * (10_000 - 1) as u128 / 10_000) as u64;
        assert_eq!(floored, 9_999);
    }

    #[test]
    fn sell_floor_rejects_degenerate() {
        assert_eq!(
            plan_sell_floor(0, 150.0, 200, 125.0, 6, &mark(2.0, 30_000.0)),
            None
        );
        assert_eq!(
            plan_sell_floor(100, 150.0, 200, 125.0, 6, &mark(0.0, 30_000.0)),
            None
        );
        assert_eq!(
            plan_sell_floor(u64::MAX, 1e-6, 0, 125.0, 6, &mark(1e12, 30_000.0)),
            None
        );
        assert_eq!(
            plan_sell_floor(100, 150.0, 200, f64::NAN, 6, &mark(2.0, 30_000.0)),
            None
        );
        assert_eq!(
            plan_sell_floor(100, 150.0, 200, -1.0, 6, &mark(2.0, 30_000.0)),
            None
        );
    }

    #[test]
    fn live_sizes_match_paper_ledger() {
        // Parity lock: same inputs size the same on both legs.
        use simulator::{PaperPortfolio, Side};
        let mut paper = PaperPortfolio::new(1_000.0, 0.0);
        let id = paper
            .open_position_params(&simulator::OpenPositionParams {
                mint: "MINT",
                side: Side::Long,
                mark: simulator::Mark::new(0.000_004_2, 30_000.0),
                notional_usd: 10.0,
                tp_percent: 75.0,
                sl_percent: -8.0,
                trailing: false,
                trail_percent: 0.0,
                timeout_seconds: 0,
                moonbag_percent: 0.0,
                now_secs: 1_000,
            })
            .expect("open");
        let qty = paper.position(id).expect("pos").quantity;
        let plan =
            plan_buy(10.0, 150.0, 200, 125.0, 6, &mark(0.000_004_2, 30_000.0)).expect("plan");
        assert!((plan.amount_tokens as i128 - qty as i128 * 1_000_000).abs() < 1_000_000);
        let floor =
            plan_sell_floor(1_000_000, 150.0, 200, 125.0, 6, &mark(3.0, 30_000.0)).expect("floor");
        let gross = 1_000_000_f64 / 1e6 * 3.0 / 150.0 * LAMPORTS_PER_SOL;
        assert!((floor as f64) < gross, "floor {floor} vs gross {gross}");
        assert!((floor as f64) > gross * 0.9, "floor {floor} vs gross {gross}");
    }

    #[test]
    fn wal_replay_folds_pending_settled_positions_sells() {
        // `DEAD` retires via its settled tombstone; `BROKEN` (missing
        // fields) and the non-JSON line are the two skips.
        let lines = [
            format!(r#"{{"t":"pending","mint":"{TEST_MINT}","curve":"{TEST_CURVE}","assoc":"{TEST_ASSOC}","amount":1000,"creator":null,"sig":"SIG1","bundle":"B1","ts":1}}"#),
            format!(r#"{{"t":"pending","mint":"{TEST_MINT_A}","curve":"{TEST_CURVE}","assoc":"{TEST_ASSOC}","amount":500,"creator":null,"sig":"SIG2","bundle":"B2","ts":2}}"#),
            r#"{"t":"settled","bundle":"B2","ts":3}"#.to_string(),
            format!(r#"{{"t":"position","mint":"{TEST_MINT_B}","curve":"{TEST_CURVE}","assoc":"{TEST_ASSOC}","amount":700,"creator":null,"bundle":"B3","ts":4}}"#),
            format!(r#"{{"t":"sell","mint":"{TEST_MINT_B}","amount":200,"bundle":"B4","ts":5}}"#),
            format!(r#"{{"t":"manual","mint":"{TEST_MINT}","curve":"{TEST_CURVE}","assoc":"{TEST_ASSOC}","amount":300,"creator":null,"ts":6}}"#),
            "not json at all".to_string(),
            r#"{"t":"pending","mint":"BROKEN","ts":7}"#.to_string(),
        ];
        let replay = wal::replay_lines(lines.iter().map(String::as_str));
        assert_eq!(replay.pending.len(), 1);
        assert_eq!(replay.pending[0].bundle_id, "B1");
        assert_eq!(replay.pending[0].signature, "SIG1");
        assert_eq!(replay.positions.len(), 1);
        assert_eq!(replay.positions[0].mint.to_string(), TEST_MINT_B);
        assert_eq!(replay.positions[0].amount_tokens, 500);
        assert_eq!(replay.manual.len(), 1);
        assert_eq!(replay.manual[0].position.mint.to_string(), TEST_MINT);
        assert_eq!(replay.manual[0].note, "");
        assert_eq!(replay.skipped, 2);
    }

    #[test]
    fn wal_replay_settled_retires_pending_beside_manual() {
        let retired = [
            format!(r#"{{"t":"pending","mint":"{TEST_MINT}","curve":"{TEST_CURVE}","assoc":"{TEST_ASSOC}","amount":1000,"creator":null,"sig":"SIG1","bundle":"B1","ts":1}}"#),
            format!(r#"{{"t":"manual","mint":"{TEST_MINT}","curve":"{TEST_CURVE}","assoc":"{TEST_ASSOC}","amount":1000,"creator":null,"note":"dispatch-unknown","ts":2}}"#),
            r#"{"t":"settled","bundle":"B1","ts":3}"#.to_string(),
        ];
        let replay = wal::replay_lines(retired.iter().map(String::as_str));
        assert!(replay.pending.is_empty(), "tombstone retires the poll");
        assert_eq!(replay.manual.len(), 1);
        assert_eq!(replay.manual[0].position.mint.to_string(), TEST_MINT);
    }

    fn cost_trader(tip_lamports: u64, unit_limit: u32, unit_price: u64) -> LiveTrader {
        use executor::WalletGuard;
        use solana_commitment_config::CommitmentConfig;
        use solana_keypair::Keypair;
        let keypair = Keypair::new();
        let wallet =
            WalletGuard::from_base58(&bs58::encode(keypair.to_bytes()).into_string())
                .expect("wallet");
        let exec = SwapExecutor::new(
            listener::types::DEFAULT_HTTP_RPC_URL,
            CommitmentConfig::confirmed(),
            wallet,
        )
        .expect("executor");
        LiveTrader::new(exec, 200, tip_lamports).with_dispatch_fees(unit_limit, unit_price)
    }

    #[test]
    fn wal_write_failure_latches_entries_off() {
        // A WAL write failure cannot unsend the dispatch, but it must
        // latch: no further entry opens while custody is unrecorded.
        let dir = std::env::temp_dir().join(format!("pump-live-wal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        // A WAL path under a regular file: create_dir_all and open both
        // fail, so every append is a write failure.
        let blocked = dir.join("not-a-dir");
        std::fs::write(&blocked, b"").expect("file");
        let trader = cost_trader(10_000, 0, 0).with_wal(blocked.join("wal.jsonl"));
        assert!(!trader.wal_degraded(), "clean start");
        trader.acknowledge_buy(
            &LivePosition {
                mint: test_pubkey(7),
                curve: test_pubkey(8),
                assoc_curve: test_pubkey(9),
                amount_tokens: 1_000,
                creator: None,
            },
            "B1",
        );
        assert!(trader.wal_degraded(), "failed write latches entries off");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn adaptive_slippage_off_by_default() {
        // Defaults: bit-identical to static slippage (200).
        let trader = cost_trader(10_000, 0, 0);
        trader.note_mark("M", 1.0, 1000);
        trader.note_mark("M", 2.0, 1001);
        assert_eq!(trader.effective_slippage_bps("M", 1001), 200);
        assert_eq!(trader.effective_slippage_bps("UNKNOWN", 1001), 200);
    }

    #[test]
    fn adaptive_slippage_tracks_window_range_capped() {
        let trader = cost_trader(10_000, 0, 0).with_adaptive_slippage(60, 150);
        // Thin history: base.
        trader.note_mark("M", 1.0, 1000);
        assert_eq!(trader.effective_slippage_bps("M", 1000), 200);
        // 10% range in window: +1000bps raw, capped at +150.
        trader.note_mark("M", 1.1, 1010);
        assert_eq!(trader.effective_slippage_bps("M", 1010), 350);
        // Small range: exact add-on (1% -> +100bps).
        let calm = cost_trader(10_000, 0, 0).with_adaptive_slippage(60, 150);
        calm.note_mark("M", 1.0, 1000);
        calm.note_mark("M", 1.01, 1010);
        assert_eq!(calm.effective_slippage_bps("M", 1010), 300);
        // Window expiry: stale marks fall out, back to base.
        assert_eq!(trader.effective_slippage_bps("M", 2000), 200);
        // Degenerate marks never poison the range.
        trader.note_mark("M", 0.0, 2001);
        trader.note_mark("M", f64::NAN, 2001);
        assert_eq!(trader.effective_slippage_bps("M", 2001), 200);
    }

    #[test]
    fn paper_legs_include_priority_fee() {
        // Paper bleed must mirror live debiting: the priority leg is part
        // of every live dispatch, so the calm-weather estimate carries it
        // too (escalated tips under congestion stay live-only: the legs
        // are a floor, never a ceiling).
        let (open, close) = paper_dispatch_legs_usd(100_000, 200, 150.0);
        let unit = 150.0 / LAMPORTS_PER_SOL;
        assert!(((open - (100_000 + 200 + BASE_TX_FEE_LAMPORTS + ATA_RENT_LAMPORTS) as f64 * unit).abs()) < 1e-9);
        assert!(((close - (100_000 + 200 + BASE_TX_FEE_LAMPORTS) as f64 * unit).abs()) < 1e-9);
        // Priority matches the live cost path exactly (one formula).
        assert_eq!(
            priority_fee_lamports(200_000, 1_000),
            cost_trader(10_000, 200_000, 1_000).dispatch_cost_for_tip(10_000)
                - 10_000
                - BASE_TX_FEE_LAMPORTS
        );
        // Priceless stays zero, never NaN bleed.
        assert_eq!(paper_dispatch_legs_usd(100_000, 200, 0.0), (0.0, 0.0));
        assert_eq!(paper_dispatch_legs_usd(100_000, 200, f64::NAN), (0.0, 0.0));
    }

    #[test]
    fn dispatch_cost_clamps_priority_fee() {
        // An absurd priority price clamps instead of minting a
        // multi-SOL cost estimate.
        let wild = cost_trader(10_000, u32::MAX, u64::MAX);
        assert_eq!(
            wild.dispatch_cost_for_tip(10_000),
            10_000 + u64::from(u32::MAX) + BASE_TX_FEE_LAMPORTS
        );
    }

    #[test]
    fn dispatch_cost_includes_base_fee() {
        let trader = cost_trader(10_000, 200_000, 1_000);
        assert_eq!(
            trader.dispatch_cost_lamports(),
            10_000 + 200 + BASE_TX_FEE_LAMPORTS
        );
        let bare = cost_trader(0, 0, 0);
        assert_eq!(bare.dispatch_cost_lamports(), BASE_TX_FEE_LAMPORTS);
    }

    #[test]
    fn first_touch_adds_ata_rent() {
        let trader = cost_trader(10_000, 200_000, 1_000);
        assert_eq!(
            trader.first_touch_cost_lamports(),
            trader.dispatch_cost_lamports() + ATA_RENT_LAMPORTS
        );
        let bare = cost_trader(0, 0, 0);
        assert_eq!(
            bare.first_touch_cost_lamports(),
            BASE_TX_FEE_LAMPORTS + ATA_RENT_LAMPORTS
        );
    }

    /// Paper fills quote the same curve the chain does: pin the
    /// simulator's integer mirrors against `executor::quote` on the
    /// shared domain. A single fee leg (creatorless curve) matches bit
    /// for bit; a split tier differs by at most one lamport (paper
    /// ceils the total once, the chain ceils each leg — the documented
    /// single-ceil convention, always conservative on the guard side).
    #[test]
    fn paper_curve_math_pins_executor_quote() {
        use executor::quote::{
            CurveView, QuoteKind, buy_cost_incl_fees, buy_tokens_for_exact_in, sell_out_after_fees,
        };
        use executor::{FeeSchedule, FeesBps};
        let sched = |proto: u64, creator: u64| {
            let leg = FeesBps {
                lp: 0,
                protocol: proto,
                creator,
            };
            FeeSchedule {
                tiers: vec![(0u128, leg)],
                stable: vec![(0u128, leg)],
                flat: leg,
                exotic: FeesBps::default(),
            }
        };
        const VQ: u64 = 30_000_000_000;
        const VT: u64 = 1_073_000_000_000_000;
        const REAL: u64 = 793_100_000_000_000;
        const SUPPLY: u64 = 1_000_000_000_000_000;
        let view = CurveView {
            vt: VT,
            vq: VQ,
            real_tokens: REAL,
            coin_creator_fee_bps: 0,
            creator_is_default: false,
            quote: QuoteKind::SolLike,
        };
        let curve = simulator::CurveReserves {
            vq_lamports: VQ,
            vt_base: VT,
            real_tokens: REAL,
            decimals: 6,
            sol_usd: 200.0,
        };
        for tier in [0u64, 1, 125, 250, 1000] {
            for sol in [1u64, 999, 10_000, 50_000_000, 200_000_000, 5_000_000_000] {
                // One fee leg: bit-for-bit on all three operations.
                let one = sched(tier, 0);
                let paper_tokens = curve.buy_tokens_base(sol, tier as f64).expect("absorbed");
                let chain_tokens =
                    buy_tokens_for_exact_in(&one, false, &view, SUPPLY, sol).expect("chain");
                assert_eq!(paper_tokens, chain_tokens, "tier={tier} sol={sol}");
                let paper_cost = curve
                    .buy_cost_lamports(paper_tokens, tier as f64)
                    .expect("cost");
                assert_eq!(
                    paper_cost,
                    buy_cost_incl_fees(&one, false, &view, SUPPLY, paper_tokens)
                        .expect("chain cost"),
                    "tier={tier} sol={sol}"
                );
                assert_eq!(
                    curve
                        .sell_net_lamports(paper_tokens, tier as f64)
                        .expect("net"),
                    sell_out_after_fees(&one, false, &view, SUPPLY, paper_tokens)
                        .expect("chain net"),
                    "tier={tier} sol={sol}"
                );
                // Split tier: two ceils vs one, bounded by a lamport on
                // the cost side; the token count can shift by one
                // lamport of curve depth (vt/vq base units).
                let split = sched(tier / 2, tier - tier / 2);
                let chain_tokens =
                    buy_tokens_for_exact_in(&split, false, &view, SUPPLY, sol).expect("chain");
                let headroom = (VT / VQ) + 2;
                assert!(
                    paper_tokens.abs_diff(chain_tokens) <= headroom,
                    "tier={tier} sol={sol}: paper={paper_tokens} chain={chain_tokens}"
                );
                let chain_cost =
                    buy_cost_incl_fees(&split, false, &view, SUPPLY, paper_tokens).expect("cost");
                assert!(
                    chain_cost >= paper_cost && chain_cost - paper_cost <= 1,
                    "tier={tier} sol={sol}: paper={paper_cost} chain={chain_cost}"
                );
                let paper_net = curve
                    .sell_net_lamports(paper_tokens, tier as f64)
                    .expect("net");
                let chain_net =
                    sell_out_after_fees(&split, false, &view, SUPPLY, paper_tokens).expect("net");
                assert!(
                    paper_net >= chain_net && paper_net - chain_net <= 1,
                    "tier={tier} sol={sol}: paper={paper_net} chain={chain_net}"
                );
            }
        }
        // A curve-draining ticket: the chain clamps its token output to
        // the real remainder, paper refuses outright (a drain means
        // migration, and paper cannot price the venue that follows).
        let draining = 5_000_000_000_000u64;
        assert_eq!(curve.buy_tokens_base(draining, 125.0), None);
        assert!(buy_tokens_for_exact_in(&sched(125, 0), false, &view, SUPPLY, draining).is_ok());
    }

    /// Sizing with reserves quotes the chain's own math: the plan's
    /// tokens and spend guard come from `CurveReserves`, and the sell
    /// floor prices the exact net for the size (never a proxy estimate).
    #[test]
    fn exact_reserves_drive_plan_buy_and_sell_floor() {
        let curve = simulator::CurveReserves {
            vq_lamports: 30_000_000_000,
            vt_base: 1_073_000_000_000_000,
            real_tokens: 793_100_000_000_000,
            decimals: 6,
            sol_usd: 150.0,
        };
        let exact = curve_mark(4.2e-6, 20_000.0, curve);
        let plan = plan_buy(10.0, 150.0, 200, 125.0, 6, &exact).expect("plan");
        let lamports = (10.0f64 / 150.0 * LAMPORTS_PER_SOL).floor() as u64;
        assert_eq!(
            plan.amount_tokens,
            curve.buy_tokens_base(lamports, 125.0).expect("tokens")
        );
        let cost = curve
            .buy_cost_lamports(plan.amount_tokens, 125.0)
            .expect("cost");
        assert_eq!(
            plan.max_sol_cost,
            apply_slippage_ceiling(cost, 200).expect("ceiling")
        );
        assert!(plan.max_sol_cost > cost);
        let floor =
            plan_sell_floor(plan.amount_tokens, 150.0, 200, 125.0, 6, &exact).expect("floor");
        let net = curve
            .sell_net_lamports(plan.amount_tokens, 125.0)
            .expect("net");
        assert_eq!(floor, (u128::from(net) * 9_800 / 10_000) as u64);
        assert!(floor > 0 && floor < net);
        // Drained reserves price nothing on either side (fail closed).
        let drained = simulator::CurveReserves {
            vq_lamports: 0,
            ..curve
        };
        let dead = curve_mark(4.2e-6, 20_000.0, drained);
        assert_eq!(plan_buy(10.0, 150.0, 200, 125.0, 6, &dead), None);
        assert_eq!(
            plan_sell_floor(plan.amount_tokens, 150.0, 200, 125.0, 6, &dead),
            None
        );
    }
}

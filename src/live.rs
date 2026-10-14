//! Live execution sink: USD sizing math plus Jito-routed dispatch.
//!
//! [`LiveTrader`] wraps [`SwapExecutor`](SwapExecutor) with the
//! pipeline's USD-denominated sizing. [`plan_buy`] / [`plan_sell`] are pure
//! (unit-tested); only [`LiveTrader::buy`] / [`sell`](LiveTrader::sell)
//! touch the network.

use crate::settings::now_secs;
use executor::{BuyOrder, ConfirmOutcome, ExecutorError, JitoReceipt, SellOrder, SwapExecutor, parse_pubkey};
use simulator::FillModel;
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;

/// Pump.fun mint decimals (base-unit scale). The confirm path verifies
/// every mint reads 6 before sizing (see `ReserveFeed::token_decimals`
/// and the decimals gate in `confirm_open`): non-6 mints are dropped
/// rather than mispriced, so this constant is a checked invariant, not
/// an assumption.
pub const TOKEN_DECIMALS: u32 = 6;
/// Lamports per SOL.
pub const LAMPORTS_PER_SOL: f64 = 1_000_000_000.0;
/// Pump.fun venue fee priced into every spend guard (protocol + creator,
/// 125 bps total — matches the simulator's per-side fee and the on-chain
/// fee schedule). Applied BEFORE the slippage ceiling: the chain debits
/// trade + fees, so a fee-blind guard spends the slippage budget on
/// baseline cost (a 1.15%-short guard failed live with 6002).
pub const VENUE_FEE_BPS: u64 = 125;

/// Live-trade failures: bad inputs or dispatch errors.
#[derive(Error, Debug)]
pub enum LiveError {
    /// Address or amount cannot be built from market data: {0}
    #[error("bad live order: {0}")]
    BadOrder(String),
    /// Dispatch failed: {0}
    #[error("dispatch failed: {0}")]
    Dispatch(#[from] ExecutorError),
    /// Bundle accepted but never confirmed. Proven on-chain failures
    /// carry the `Confirmed failure:` prefix (never rechecked; no fill);
    /// everything else is pending (rechecked until proven).
    #[error("unconfirmed live fill: {0}")]
    Unconfirmed(String),
    /// Dispatch timed out with no signature: the bundle may or may not
    /// have reached the engine. Carries the intended position so the
    /// pipeline can park it for manual review instead of losing it.
    /// Boxed: `LivePosition` holds five `String`s, and an unboxed
    /// variant trips `clippy::result_large_err` on every live call.
    #[error("unknown dispatch fill: {reason}")]
    UnknownDispatch {
        /// Intended fill (amounts as sized, fill unknown).
        position: Box<LivePosition>,
        /// Human-readable cause.
        reason: String,
    },
}

/// Fill-confirmation deadline per dispatch (bundles land in seconds or
/// not at all; the gate loop must not stall on one token).
pub const LIVE_CONFIRM_TIMEOUT: Duration = Duration::from_secs(60);
/// Dispatch deadline per Jito send: only `confirm_signature` used to be
/// bounded, so a hung RPC/Jito call blocked the caller (and its locks)
/// forever. A timed-out dispatch has no receipt, so the fill is reported
/// unknown for manual review rather than assumed either way.
pub const LIVE_DISPATCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Minimum spacing between live-sell retries for one mint.
pub const SELL_RETRY_COOLDOWN: Duration = Duration::from_secs(30);
/// Failed live sells before tracking is dropped for manual exit.
pub const SELL_MAX_ATTEMPTS: u32 = 5;
/// Recheck rounds for an unconfirmed buy before giving up to manual review.
pub const PENDING_MAX_ATTEMPTS: u32 = 5;

/// Tracked live fill (amounts in token base units).
#[derive(Debug, Clone)]
pub struct LivePosition {
    /// Token mint (base58).
    pub mint: String,
    /// Bonding-curve address (base58).
    pub curve: String,
    /// Curve ATA address (base58).
    pub assoc_curve: String,
    /// Bought base units (for full-size exits).
    pub amount_tokens: u64,
    /// Token creator (base58): required for v3 dispatch (cross-checked
    /// against curve state). `None` fails closed downstream.
    pub creator: Option<String>,
}

/// Buy request: sizing inputs plus the optional creator, bundled so
/// live-dispatch context cannot grow call arity again.
#[derive(Debug, Clone, Copy)]
pub struct LiveBuyParams<'a> {
    /// Token mint (base58).
    pub mint: &'a str,
    /// Bonding-curve address (base58).
    pub curve: &'a str,
    /// Curve ATA address (base58).
    pub assoc_curve: &'a str,
    /// Exposure in USD.
    pub notional_usd: f64,
    /// Execution price in USD per token.
    pub price_usd: f64,
    /// SOL price in USD (lamport conversion).
    pub sol_usd: f64,
    /// Curve liquidity in USD for impact sizing (mirrors the paper
    /// [`FillModel`](FillModel): unknown/zero pays fee-only).
    pub liquidity_usd: f64,
    /// Token creator (base58), when the launch carried one.
    pub creator: Option<&'a str>,
}

/// Planned buy: integer amounts ready for [`BuyOrder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveBuyPlan {
    /// Token base units to buy.
    pub amount_tokens: u64,
    /// Slippage guard in lamports.
    pub max_sol_cost: u64,
}

/// Guard ceiling over a fee-inclusive quote: `quoted_cost_incl_fees`
/// plus `slippage_bps`, rounded UP (a floored guard underfunds the buy
/// by a lamport and fails with 6002 `TooMuchSolRequired` — the frozen
/// failed sample missed by 18,969 lamports, 1.15%). `None` on zero
/// quotes and on `u64` overflow: an unguardable cost fails closed,
/// never dispatches unprotected. `slippage_bps` is clamped to 10_000
/// like every other sizing path. This is THE guard constructor: once a
/// fee-inclusive on-chain quote engine lands, its output feeds this
/// function directly (fees must never be added twice — the quote
/// already includes protocol+creator fees, like the 1_668_970-lamport
/// `Right` in the frozen 6002 log).
pub fn apply_slippage_ceiling(quoted_cost_incl_fees: u64, slippage_bps: u64) -> Option<u64> {
    // Single constructor lives in the executor crate so binary and
    // executor can never drift apart; this wrapper keeps the
    // pipeline-facing name stable.
    executor::quote::slippage_ceiling(quoted_cost_incl_fees, slippage_bps)
}

/// Size a buy from USD notional at spot: tokens from `notional/price`
/// through the paper [`FillModel`](FillModel) (fee + impact,
/// fee-only when liquidity is unknown — identical inputs give identical
/// sizes on both legs), spend cap from `notional/sol` plus
/// `slippage_bps` (clamped to 10_000 = 100% like the sell floor, so a
/// misconfigured `SLIPPAGE_BPS` cannot authorize unbounded overspend).
/// `None` on non-positive inputs, zero quantity, or amounts that do not
/// fit `u64` (`as`-casts saturate silently past the max — a garbage
/// print must fail closed, never size a jumbo order).
pub fn plan_buy(
    notional_usd: f64,
    price_usd: f64,
    sol_usd: f64,
    slippage_bps: u64,
    liquidity_usd: f64,
) -> Option<LiveBuyPlan> {
    if !notional_usd.is_finite() || notional_usd <= 0.0 {
        return None;
    }
    if !price_usd.is_finite() || price_usd <= 0.0 {
        return None;
    }
    if !sol_usd.is_finite() || sol_usd <= 0.0 {
        return None;
    }
    // Paper parity: the ledger fills at quote*(1+slip), so size from the
    // filled price (floored — never oversize custody past 2^53 either).
    let slip = FillModel::default().slip_fraction(notional_usd, liquidity_usd);
    let scale = 10f64.powi(TOKEN_DECIMALS as i32);
    let amount_f = (notional_usd / price_usd / (1.0 + slip) * scale).floor();
    if !amount_f.is_finite() || amount_f <= 0.0 || amount_f > u64::MAX as f64 {
        return None;
    }
    let amount_tokens = amount_f as u64;
    if amount_tokens == 0 {
        return None;
    }
    let lamports = notional_usd / sol_usd * LAMPORTS_PER_SOL;
    if !lamports.is_finite() || lamports <= 0.0 || lamports > u64::MAX as f64 {
        return None;
    }
    // Venue fee first (exact integer ceil in u128; the narrowing
    // `try_from` fails closed if the fee tips a near-max guard over
    // u64::MAX), then the slippage ceiling. The fee engine downstream
    // recomputes fee-inclusively and only ever RAISES this guard, so
    // pre-paying the fee here never double-counts.
    let with_fee = u64::try_from(
        (lamports as u64 as u128 * (10_000 + VENUE_FEE_BPS) as u128).div_ceil(10_000),
    )
    .ok()?;
    // One guard constructor everywhere (ceiling math): the float quote
    // here stands in for a fee-inclusive on-chain quote until the quote
    // engine lands — the helper, not ad-hoc math, sets every guard.
    let max_sol_cost = apply_slippage_ceiling(with_fee, slippage_bps)?;
    Some(LiveBuyPlan {
        amount_tokens,
        max_sol_cost,
    })
}

/// Floor for a sell: model proceeds (paper [`FillModel`](FillModel)
/// at the same inputs, so the floor tracks what the ledger books) minus
/// `slippage_bps`. `None` on degenerate inputs — and on a zero floor:
/// `min_sol_output = 0` authorizes the slot leader to fill at any price
/// (sandwich drain), so full-slippage configs and sub-lamport dust fail
/// closed instead of dispatching unprotected.
pub fn plan_sell_floor(
    amount_tokens: u64,
    price_usd: f64,
    sol_usd: f64,
    slippage_bps: u64,
    liquidity_usd: f64,
) -> Option<u64> {
    if amount_tokens == 0 || !price_usd.is_finite() || price_usd <= 0.0 {
        return None;
    }
    if !sol_usd.is_finite() || sol_usd <= 0.0 {
        return None;
    }
    if slippage_bps >= 10_000 {
        return None;
    }
    let scale = 10f64.powi(TOKEN_DECIMALS as i32);
    // Paper parity: fills land BELOW the quote by the model slip, so the
    // floor starts from modeled proceeds (a bare-quote floor would price
    // above reality and fail the fill on-chain).
    let gross_notional = amount_tokens as f64 / scale * price_usd;
    let slip = FillModel::default().slip_fraction(gross_notional, liquidity_usd);
    let lamports = gross_notional * (1.0 - slip).max(0.0) / sol_usd * LAMPORTS_PER_SOL;
    if !lamports.is_finite() || lamports <= 0.0 || lamports > u64::MAX as f64 {
        return None;
    }
    // `as u64` saturates past the max — the check above fails closed
    // first, like `plan_buy` (a saturating floor would authorize a jumbo
    // `min_sol_output`... which fails closed downstream anyway, but an
    // unguardable quote must never dispatch).
    let floor = (lamports * (10_000.0 - slippage_bps as f64) / 10_000.0).floor() as u64;
    if floor == 0 {
        return None;
    }
    Some(floor)
}

/// Jito-routed live trader over a configured [`SwapExecutor`].
#[derive(Clone)]
pub struct LiveTrader {
    executor: SwapExecutor,
    slippage_bps: u64,
    tip_lamports: u64,
    /// Priority-fee price for cost accounting (set from the same knobs
    /// as the executor's compute budget; see `with_dispatch_fees`).
    priority_fee_micro_lamports: u64,
    /// Compute-unit limit for cost accounting.
    compute_unit_limit: u32,
    /// Write-ahead log for crash/restart recovery (see [`WalReplay`]).
    /// `None` (tests, paper) disables all WAL IO.
    wal_path: Option<PathBuf>,
    /// Buys accepted by the engine but unconfirmed when the deadline hit.
    /// A timed-out bundle may still land late; dropping it here would leave
    /// real tokens untracked (no exit automation). Entries are re-polled
    /// via [`LiveTrader::recheck_pending`] until confirmed, failed, or
    /// out of attempts (then manual review). Short critical sections only.
    pending: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, PendingBuy>>>,
    /// At most one [`LiveTrader::recheck_pending`] round runs at a time:
    /// ticks fire every second while a round can take up to the confirm
    /// timeout, so overlapping rounds would stack identical confirm RPCs
    /// and race the attempt counters. Late callers skip (empty vec).
    recheck_busy: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Buy awaiting confirmation: reserved for the recheck loop, never treated
/// as a fill until the chain says so. Carries BOTH identifiers: the
/// base58 `signature` (what `confirm_signature` polls — a bundle UUID
/// never parses as one) and the engine `bundle_id` (for logs/cards).
#[derive(Debug, Clone)]
struct PendingBuy {
    position: LivePosition,
    signature: String,
    bundle_id: String,
    attempts: u32,
}

/// Outcome of one [`LiveTrader::recheck_pending`] round per mint.
#[derive(Debug, Clone)]
pub struct PendingRecheck {
    /// Expected position (valid only when `outcome` is `Confirmed`).
    pub position: LivePosition,
    /// Engine bundle UUID.
    pub bundle_id: String,
    /// Transaction signature (what to look up on-chain on GiveUp).
    pub signature: String,
    /// What the re-poll found.
    pub outcome: RecheckOutcome,
}

/// Recheck verdict for one pending buy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecheckOutcome {
    /// Landed after all: promote to tracked and automate exits.
    Confirmed,
    /// Errored on-chain: nothing held, drop silently-ish (log).
    FailedOnChain,
    /// Still unknown: keep pending for the next round.
    StillUnknown,
    /// Out of attempts: drop and escalate to manual review.
    GiveUp,
}

/// Crash-recovery write-ahead log (JSONL, hand-rolled like
/// [`crate::cli::PidInfo`]: base58 values never need escaping).
///
/// Every custody-changing event appends BEFORE the in-memory map is
/// touched, so a SIGKILL/abort between dispatch and bookkeeping replays
/// to the same state on the next boot instead of stranding funds:
///
/// - `pending` after a bundle receipt (re-polled on boot),
/// - `settled` once a pending bundle is proven failed (stops re-poll),
/// - `position` once a fill is tracked (restores `live_positions`),
/// - `sell` once a sell confirms (subtracts on replay),
/// - `manual` once a bag parks for human exit (restores `live_manual`).
///
/// Boot replays in file order (ordered fold) and compacts back to the
/// surviving state. Corrupt lines are skipped with a loud log, never a
/// panic. `None` path (tests, paper) disables all WAL IO.
pub mod wal {
    use super::LivePosition;

    /// One replayed pending buy: position plus both identifiers.
    #[derive(Debug, Clone)]
    pub struct WalPending {
        /// Intended fill.
        pub position: LivePosition,
        /// Transaction signature for `confirm_signature` re-polls.
        pub signature: String,
        /// Engine bundle UUID (dedup key).
        pub bundle_id: String,
    }

    /// One replayed human-exit bag: position plus its lookup note.
    #[derive(Debug, Clone)]
    pub struct WalManual {
        /// Held bag.
        pub position: LivePosition,
        /// Lookup identifier (bundle, signature, or cause).
        pub note: String,
    }

    /// Replayed boot state.
    #[derive(Debug, Default, Clone)]
    pub struct WalReplay {
        /// Pending buys whose bundle never settled (recheck on boot).
        pub pending: Vec<WalPending>,
        /// Tracked fills (`live_positions`).
        pub positions: Vec<LivePosition>,
        /// Human-exit bags (`live_manual`).
        pub manual: Vec<WalManual>,
        /// Corrupt lines skipped (operator should inspect the file).
        pub skipped: usize,
    }

    fn field(line: &str, key: &str) -> Option<String> {
        for part in line
            .trim()
            .strip_prefix('{')?
            .strip_suffix('}')?
            .split(',')
        {
            let (k, v) = part.split_once(':')?;
            if k.trim().trim_matches('"') == key {
                let v = v.trim();
                if v == "null" {
                    return None;
                }
                return Some(v.trim_matches('"').to_string());
            }
        }
        None
    }

    fn num(line: &str, key: &str) -> Option<u64> {
        field(line, key)?.parse().ok()
    }

    fn position_from(line: &str) -> Option<LivePosition> {
        Some(LivePosition {
            mint: field(line, "mint")?,
            curve: field(line, "curve")?,
            assoc_curve: field(line, "assoc")?,
            amount_tokens: num(line, "amount")?,
            creator: field(line, "creator"),
        })
    }

    /// Fold WAL lines (in order) into boot state. Pure over lines: the
    /// file read stays in `replay_file` so this is unit-tested directly.
    pub fn replay_lines<'a>(lines: impl Iterator<Item = &'a str>) -> WalReplay {
        use std::collections::HashMap;
        let mut pending: HashMap<String, WalPending> = HashMap::new();
        let mut positions: HashMap<String, LivePosition> = HashMap::new();
        let mut manual: HashMap<String, WalManual> = HashMap::new();
        let mut skipped = 0usize;
        let add = |map: &mut HashMap<String, LivePosition>, p: LivePosition| {
            map.entry(p.mint.clone())
                .and_modify(|e| {
                    e.amount_tokens = e.amount_tokens.saturating_add(p.amount_tokens);
                })
                .or_insert(p);
        };
        for line in lines {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Some(kind) = field(line, "t") else {
                skipped += 1;
                continue;
            };
            match kind.as_str() {
                "pending" => {
                    let (Some(pos), sig, bundle) = (
                        position_from(line),
                        field(line, "sig"),
                        field(line, "bundle"),
                    ) else {
                        skipped += 1;
                        continue;
                    };
                    let (Some(signature), Some(bundle_id)) = (sig, bundle) else {
                        skipped += 1;
                        continue;
                    };
                    pending.insert(
                        bundle_id.clone(),
                        WalPending { position: pos, signature, bundle_id },
                    );
                }
                "settled" => {
                    let Some(bundle) = field(line, "bundle") else {
                        skipped += 1;
                        continue;
                    };
                    pending.remove(&bundle);
                }
                "position" => {
                    let (Some(pos), bundle) = (position_from(line), field(line, "bundle")) else {
                        skipped += 1;
                        continue;
                    };
                    let _ = bundle;
                    add(&mut positions, pos);
                }
                "sell" => {
                    let (Some(mint), Some(amount)) = (field(line, "mint"), num(line, "amount"))
                    else {
                        skipped += 1;
                        continue;
                    };
                    if let Some(p) = positions.get_mut(&mint) {
                        p.amount_tokens = p.amount_tokens.saturating_sub(amount);
                        if p.amount_tokens == 0 {
                            positions.remove(&mint);
                        }
                    }
                }
                "manual" => {
                    let Some(pos) = position_from(line) else {
                        skipped += 1;
                        continue;
                    };
                    // Pre-note WALs (and hand-written lines) carry no
                    // note: default empty, never skip a real bag.
                    let note = field(line, "note").unwrap_or_default();
                    manual
                        .entry(pos.mint.clone())
                        .and_modify(|e: &mut WalManual| {
                            e.position.amount_tokens =
                                e.position.amount_tokens.saturating_add(pos.amount_tokens);
                        })
                        .or_insert(WalManual { position: pos, note });
                }
                _ => skipped += 1,
            }
        }
        WalReplay {
            pending: pending.into_values().collect(),
            positions: positions.into_values().collect(),
            manual: manual.into_values().collect(),
            skipped,
        }
    }

    /// Read + replay a WAL file. Missing file = empty replay (first
    /// boot), never an error. Corrupt lines count in `skipped`.
    pub fn replay_file(path: &std::path::Path) -> WalReplay {
        let raw = std::fs::read_to_string(path).unwrap_or_default();
        let replay = replay_lines(raw.lines());
        if replay.skipped > 0 {
            tracing::warn!(file = %path.display(), skipped = replay.skipped, "live WAL has corrupt lines; skipped (inspect the file)");
        }
        replay
    }

    /// Compact back to the surviving state (pending + positions +
    /// manual). Called once per boot after import so the file stays
    /// bounded; in-run appends keep it current after that.
    pub fn compact_file(path: &std::path::Path, replay: &WalReplay) {
        use std::fmt::Write as _;
        let mut out = String::new();
        for p in &replay.pending {
            let _ = writeln!(
                out,
                "{{\"t\":\"pending\",\"mint\":\"{}\",\"curve\":\"{}\",\"assoc\":\"{}\",\"amount\":{},\"creator\":{},\"sig\":\"{}\",\"bundle\":\"{}\",\"ts\":{}}}",
                p.position.mint,
                p.position.curve,
                p.position.assoc_curve,
                p.position.amount_tokens,
                p.position
                    .creator
                    .as_ref()
                    .map(|c| format!("\"{c}\""))
                    .unwrap_or_else(|| "null".to_string()),
                p.signature,
                p.bundle_id,
                super::now_secs(),
            );
        }
        for p in &replay.positions {
            let _ = writeln!(
                out,
                "{{\"t\":\"position\",\"mint\":\"{}\",\"curve\":\"{}\",\"assoc\":\"{}\",\"amount\":{},\"creator\":{},\"ts\":{}}}",
                p.mint,
                p.curve,
                p.assoc_curve,
                p.amount_tokens,
                p.creator
                    .as_ref()
                    .map(|c| format!("\"{c}\""))
                    .unwrap_or_else(|| "null".to_string()),
                super::now_secs(),
            );
        }
        for m in &replay.manual {
            let p = &m.position;
            let _ = writeln!(
                out,
                "{{\"t\":\"manual\",\"mint\":\"{}\",\"curve\":\"{}\",\"assoc\":\"{}\",\"amount\":{},\"creator\":{},\"note\":\"{}\",\"ts\":{}}}",
                p.mint,
                p.curve,
                p.assoc_curve,
                p.amount_tokens,
                p.creator
                    .as_ref()
                    .map(|c| format!("\"{c}\""))
                    .unwrap_or_else(|| "null".to_string()),
                m.note,
                super::now_secs(),
            );
        }
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = crate::cli::write_private_file(path, out.as_bytes()) {
            tracing::warn!(file = %path.display(), error = %e, "live WAL compaction failed; file keeps growing (non-fatal)");
        }
    }
}

impl LiveTrader {
    /// Wrap an executor (already Jito-attached) with sizing policy.
    pub fn new(executor: SwapExecutor, slippage_bps: u64, tip_lamports: u64) -> Self {
        Self {
            executor,
            slippage_bps,
            tip_lamports,
            priority_fee_micro_lamports: 0,
            compute_unit_limit: 0,
            wal_path: None,
            pending: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            recheck_busy: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Enable the crash-recovery write-ahead log at `path` (live boot
    /// sets this next to the log file; tests leave it `None`).
    pub fn with_wal(mut self, path: PathBuf) -> Self {
        self.wal_path = Some(path);
        self
    }

    /// Compute-budget knobs for cost accounting (mirrors the executor's
    /// `with_compute_budget`; the trader never builds instructions).
    pub fn with_dispatch_fees(
        mut self,
        compute_unit_limit: u32,
        priority_fee_micro_lamports: u64,
    ) -> Self {
        self.compute_unit_limit = compute_unit_limit;
        self.priority_fee_micro_lamports = priority_fee_micro_lamports;
        self
    }

    /// Per-dispatch on-chain overhead in lamports: Jito tip plus the
    /// priority fee (`ceil(limit * price / 1e6)`), saturating instead of
    /// wrapping on absurd configs. The breaker prices this into USD so
    /// the day-loss gate sees live bleed, not paper-only profit.
    pub fn dispatch_cost_lamports(&self) -> u64 {
        let prio = (self.compute_unit_limit as u128)
            .saturating_mul(self.priority_fee_micro_lamports as u128)
            .saturating_add(999_999)
            / 1_000_000;
        let prio = u64::try_from(prio).unwrap_or(u64::MAX);
        self.tip_lamports.saturating_add(prio)
    }

    /// True when at least one buy awaits confirmation.
    pub fn has_pending(&self) -> bool {
        match self.pending.lock() {
            Ok(guard) => !guard.is_empty(),
            Err(_) => {
                tracing::warn!("pending-tracker lock poisoned; assuming pending");
                true
            }
        }
    }

    /// Mints pending confirmation (cloned).
    pub fn pending_mints(&self) -> Vec<String> {
        match self.pending.lock() {
            Ok(guard) => guard.keys().cloned().collect(),
            Err(_) => {
                tracing::warn!("pending-tracker lock poisoned; pending mints unknown");
                Vec::new()
            }
        }
    }

    /// Re-poll every pending buy once (one fresh confirmation window each).
    /// Confirmed and failed entries leave the map; unknown ones stay until
    /// [`PENDING_MAX_ATTEMPTS`] and then report [`RecheckOutcome::GiveUp`].
    /// Confirms run concurrently (one slow mint must not hold up the
    /// rest); a round already in flight makes late callers skip.
    pub async fn recheck_pending(&self) -> Vec<PendingRecheck> {
        if self
            .recheck_busy
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            // Ticks fire every second while a round can take up to the
            // confirm timeout: skips are routine, not lost work. Debug
            // (not warn) so a slow mint does not spam the ops log.
            tracing::debug!("pending recheck already in flight; skipping round");
            return Vec::new();
        }
        // Reset on every exit path, including panic unwinds: a stuck flag
        // would silence all future rechecks.
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
        let snapshot: Vec<PendingBuy> = match self.pending.lock() {
            Ok(guard) => guard.values().cloned().collect(),
            // Poisoned after a prior panic: report empty (nothing
            // rechecked) but loudly, never silently.
            Err(_) => {
                tracing::warn!("pending-tracker lock poisoned; recheck skipped");
                Vec::new()
            }
        };
        let results = futures_util::future::join_all(snapshot.into_iter().map(|entry| async {
            let outcome = match self
                .executor
                .confirm_signature(&entry.signature, LIVE_CONFIRM_TIMEOUT)
                .await
            {
                Ok(ConfirmOutcome::Confirmed) => RecheckOutcome::Confirmed,
                Ok(ConfirmOutcome::FailedOnChain) => RecheckOutcome::FailedOnChain,
                Ok(ConfirmOutcome::Timeout) | Err(_) => {
                    let attempts = entry.attempts.saturating_add(1);
                    if attempts >= PENDING_MAX_ATTEMPTS {
                        RecheckOutcome::GiveUp
                    } else {
                        match self.pending.lock() {
                            Ok(mut guard) => {
                                if let Some(stored) = guard.get_mut(&entry.position.mint) {
                                    stored.attempts = attempts;
                                }
                            }
                            Err(_) => tracing::warn!(
                                mint = %entry.position.mint,
                                "pending-tracker lock poisoned; attempt counter frozen"
                            ),
                        }
                        RecheckOutcome::StillUnknown
                    }
                }
            };
            (entry, outcome)
        }))
        .await;
        let mut out = Vec::with_capacity(results.len());
        for (entry, outcome) in results {
            match outcome {
                RecheckOutcome::StillUnknown => {}
                _ => match self.pending.lock() {
                    Ok(mut guard) => {
                        guard.remove(&entry.position.mint);
                    }
                    Err(_) => tracing::warn!(
                        mint = %entry.position.mint,
                        "pending-tracker lock poisoned; settled entry not removed"
                    ),
                },
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

    /// Build the tracked [`LivePosition`] for one buy (single
    /// constructor so dispatch, confirm, and pending paths cannot drift
    /// apart on what a fill carries).
    fn pending_position(
        mint: &str,
        curve: &str,
        assoc_curve: &str,
        amount_tokens: u64,
        creator: Option<String>,
    ) -> LivePosition {
        LivePosition {
            mint: mint.to_string(),
            curve: curve.to_string(),
            assoc_curve: assoc_curve.to_string(),
            amount_tokens,
            creator,
        }
    }

    /// Buy `notional_usd` at `price_usd` via Jito bundle.
    /// Returns the tracked position plus the bundle UUID.
    pub async fn buy(&self, req: &LiveBuyParams<'_>) -> Result<(LivePosition, String), LiveError> {
        let (mint, curve, assoc_curve, notional_usd, price_usd, sol_usd) = (
            req.mint, req.curve, req.assoc_curve, req.notional_usd, req.price_usd, req.sol_usd,
        );
        let creator = match req.creator {
            Some(c) => {
                Some(parse_pubkey(c).map_err(|e| LiveError::BadOrder(format!("bad creator '{c}': {e}")))?)
            }
            None => None,
        };
        let plan = plan_buy(
            notional_usd,
            price_usd,
            sol_usd,
            self.slippage_bps,
            req.liquidity_usd,
        )
        .ok_or_else(|| {
            LiveError::BadOrder(format!("cannot size buy of {mint} at {price_usd}"))
        })?;
        let receipt = match tokio::time::timeout(
            LIVE_DISPATCH_TIMEOUT,
            self.executor.buy_jito(&BuyOrder {
                mint: parse_pubkey(mint)
                    .map_err(|e| LiveError::BadOrder(format!("bad mint '{mint}': {e}")))?,
                bonding_curve: parse_pubkey(curve)
                    .map_err(|e| LiveError::BadOrder(format!("bad curve '{curve}': {e}")))?,
                associated_bonding_curve: parse_pubkey(assoc_curve)
                    .map_err(|e| LiveError::BadOrder(format!("bad assoc_curve '{assoc_curve}': {e}")))?,
                amount_tokens: plan.amount_tokens,
                max_sol_cost: plan.max_sol_cost,
                tip_lamports: self.tip_lamports,
                creator,
                slippage_bps: self.slippage_bps,
            }),
        )
        .await
        {
            Ok(dispatch) => dispatch?,
            Err(_) => {
                // Dispatch hung: the bundle may or may not have reached
                // the engine, and there is no signature to re-poll — the
                // fill is unknown and needs a human. Carry the intended
                // position so the pipeline parks it (never drops it).
                let position = Self::pending_position(
                    mint,
                    curve,
                    assoc_curve,
                    plan.amount_tokens,
                    req.creator.map(|c| c.to_string()),
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
        // Write-ahead tracking: the receipt exists from here on, so an
        // abort/crash during `confirm_signature` below must not lose it.
        // Fast-path confirm/failure removes the entry again; timeouts and
        // transport errors keep it for the recheck loop.
        let intended = Self::pending_position(
            mint,
            curve,
            assoc_curve,
            plan.amount_tokens,
            creator.map(|c| c.to_string()),
        );
        Self::track_pending(&self.pending, mint, intended.clone(), &receipt);
        self.write_wal_pending(&intended, &receipt.signature, &receipt.bundle_id);
        match self
            .executor
            .confirm_signature(&receipt.signature, LIVE_CONFIRM_TIMEOUT)
            .await
        {
            Ok(ConfirmOutcome::Confirmed) => {
                Self::untrack_pending(&self.pending, mint);
                Ok((
                    Self::pending_position(
                        mint,
                        curve,
                        assoc_curve,
                        plan.amount_tokens,
                        creator.map(|c| c.to_string()),
                    ),
                    receipt.bundle_id,
                ))
            }
            Ok(ConfirmOutcome::FailedOnChain) => {
                Self::untrack_pending(&self.pending, mint);
                // Proven no-fill: retire the WAL pending line so boot
                // replay never re-polls a dead bundle.
                self.write_wal_settled(&receipt.bundle_id);
                Err(LiveError::Unconfirmed(format!(
                    "buy bundle {} Confirmed failure: errored on-chain; no fill",
                    receipt.bundle_id
                )))
            }
            Ok(ConfirmOutcome::Timeout) => {
                // A timed-out bundle may still land late: track it for the
                // recheck loop instead of assuming no fill (an untracked bag
                // would never get exit automation).
                Self::track_pending(
                    &self.pending,
                    mint,
                    Self::pending_position(
                        mint,
                        curve,
                        assoc_curve,
                        plan.amount_tokens,
                        creator.map(|c| c.to_string()),
                    ),
                    &receipt,
                );
                Err(LiveError::Unconfirmed(format!(
                    "buy bundle {} unconfirmed after {:?}; tracked as pending, rechecking",
                    receipt.bundle_id, LIVE_CONFIRM_TIMEOUT
                )))
            }
            Err(e) => {
                // Transport failure mid-confirm: the fill is just as
                // unknown as a timeout (the bundle may already have
                // landed), so track it identically. Only a proven
                // on-chain failure means no fill.
                Self::track_pending(
                    &self.pending,
                    mint,
                    Self::pending_position(
                        mint,
                        curve,
                        assoc_curve,
                        plan.amount_tokens,
                        creator.map(|c| c.to_string()),
                    ),
                    &receipt,
                );
                Err(LiveError::Unconfirmed(format!(
                    "buy bundle {} confirm RPC failed ({e}); tracked as pending, rechecking",
                    receipt.bundle_id
                )))
            }
        }
    }

    /// Record an unconfirmed buy for the recheck loop. Used for timeouts
    /// AND transport errors alike: in both cases the fill is unknown, and
    /// only a proven on-chain failure (`FailedOnChain`) means no fill.
    /// First receipt wins per mint: a second pending buy for the same
    /// mint is logged loudly for manual review instead of silently
    /// dropping the first bag's tracking.
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
                    attempts: 1,
                });
            }
            // A poisoned lock means a prior panic already fired: losing
            // the bag's tracking silently would strand real funds without
            // exit automation — escalate loudly instead.
            Err(_) => tracing::warn!(mint = %mint, bundle = %receipt.bundle_id,
                "pending-tracker lock poisoned; buy fill untracked, MANUAL REVIEW REQUIRED"),
        }
    }

    /// Remove a write-ahead pending entry once the fast path settles
    /// (confirmed fill or proven on-chain failure). Timeouts and
    /// transport errors keep the entry for the recheck loop.
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

    /// Append one WAL line (best-effort: IO failure warns, never fails
    /// the dispatch — the in-memory map stays authoritative this run).
    /// No `fsync`: a lost tail replays as recheck-then-track (safe), and
    /// a sync per dispatch would serialize every sell through disk
    /// latency on the hot path.
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
                if writeln!(f, "{line}").is_err() {
                    tracing::warn!("live WAL append failed (non-fatal)");
                }
            }
            Err(e) => tracing::warn!(error = %e, "live WAL unwritable (non-fatal)"),
        }
    }

    /// JSON value for an optional creator (`null` when unknown).
    fn wal_creator(creator: Option<&String>) -> String {
        creator
            .map(|c| format!("\"{c}\""))
            .unwrap_or_else(|| "null".to_string())
    }

    /// Record a dispatched bundle BEFORE confirming it (write-ahead: a
    /// crash during confirm replays to recheck, never to silence).
    fn write_wal_pending(&self, position: &LivePosition, signature: &str, bundle_id: &str) {
        if self.wal_path.is_none() {
            return;
        }
        self.wal_append(format!(
            "{{\"t\":\"pending\",\"mint\":\"{}\",\"curve\":\"{}\",\"assoc\":\"{}\",\"amount\":{},\"creator\":{},\"sig\":\"{signature}\",\"bundle\":\"{bundle_id}\",\"ts\":{}}}",
            position.mint,
            position.curve,
            position.assoc_curve,
            position.amount_tokens,
            Self::wal_creator(position.creator.as_ref()),
            now_secs(),
        ));
    }

    /// Record a proven-dead bundle so boot replay stops re-polling it.
    /// (Fast-path confirms are recorded by [`LiveTrader::acknowledge_buy`]
    /// instead — only AFTER the pipeline tracks the fill, so a crash in
    /// between replays to a recheck that re-confirms rather than to loss.)
    fn write_wal_settled(&self, bundle_id: &str) {
        if self.wal_path.is_none() {
            return;
        }
        self.wal_append(format!(
            "{{\"t\":\"settled\",\"bundle\":\"{bundle_id}\",\"ts\":{}}}",
            now_secs(),
        ));
    }

    /// Record a tracked fill + retire its pending line. Call AFTER
    /// inserting into `live_positions` (write-behind for the position,
    /// so a crash replays to recheck-then-track, never to a settled
    /// tombstone over an untracked bag).
    pub fn acknowledge_buy(&self, position: &LivePosition, bundle_id: &str) {
        if self.wal_path.is_none() {
            return;
        }
        self.wal_append(format!(
            "{{\"t\":\"position\",\"mint\":\"{}\",\"curve\":\"{}\",\"assoc\":\"{}\",\"amount\":{},\"creator\":{},\"bundle\":\"{bundle_id}\",\"ts\":{}}}",
            position.mint,
            position.curve,
            position.assoc_curve,
            position.amount_tokens,
            Self::wal_creator(position.creator.as_ref()),
            now_secs(),
        ));
        self.write_wal_settled(bundle_id);
    }

    /// Record a human-exit bag (write-ahead of the `live_manual` insert).
    /// `note` is a lookup identifier (bundle, signature, or cause) with
    /// no quotes/backslashes/commas (base58/UUID/plain words only).
    pub fn record_manual(&self, position: &LivePosition, note: &str) {
        if self.wal_path.is_none() {
            return;
        }
        self.wal_append(format!(
            "{{\"t\":\"manual\",\"mint\":\"{}\",\"curve\":\"{}\",\"assoc\":\"{}\",\"amount\":{},\"creator\":{},\"note\":\"{note}\",\"ts\":{}}}",
            position.mint,
            position.curve,
            position.assoc_curve,
            position.amount_tokens,
            Self::wal_creator(position.creator.as_ref()),
            now_secs(),
        ));
    }

    /// Record a confirmed sell (write-ahead of the custody subtract).
    pub fn record_sell(&self, mint: &str, amount_tokens: u64, bundle_id: &str) {
        if self.wal_path.is_none() {
            return;
        }
        self.wal_append(format!(
            "{{\"t\":\"sell\",\"mint\":\"{mint}\",\"amount\":{amount_tokens},\"bundle\":\"{bundle_id}\",\"ts\":{}}}",
            now_secs(),
        ));
    }

    /// Import boot-replayed pendings into the recheck loop.
    pub fn import_pending(&self, items: Vec<wal::WalPending>) -> usize {
        let mut n = 0usize;
        match self.pending.lock() {
            Ok(mut guard) => {
                for item in items {
                    guard.entry(item.position.mint.clone()).or_insert(PendingBuy {
                        position: item.position,
                        signature: item.signature,
                        bundle_id: item.bundle_id,
                        attempts: 1,
                    });
                    n += 1;
                }
            }
            Err(_) => tracing::warn!("pending-tracker lock poisoned; WAL import dropped"),
        }
        n
    }

    /// Sell the full tracked amount at `price_usd` via Jito bundle.
    /// Returns the bundle UUID.
    pub async fn sell(
        &self,
        position: &LivePosition,
        price_usd: f64,
        sol_usd: f64,
        liquidity_usd: f64,
    ) -> Result<String, LiveError> {
        let floor = plan_sell_floor(
            position.amount_tokens,
            price_usd,
            sol_usd,
            self.slippage_bps,
            liquidity_usd,
        )
        .ok_or_else(|| LiveError::BadOrder(format!("cannot floor sell of {}", position.mint)))?;
        let receipt = match tokio::time::timeout(
            LIVE_DISPATCH_TIMEOUT,
            self.executor.sell_jito(&SellOrder {
                mint: parse_pubkey(&position.mint)
                    .map_err(|e| LiveError::BadOrder(format!("bad mint '{}': {e}", position.mint)))?,
                bonding_curve: parse_pubkey(&position.curve)
                    .map_err(|e| LiveError::BadOrder(format!("bad curve '{}': {e}", position.curve)))?,
                associated_bonding_curve: parse_pubkey(&position.assoc_curve)
                    .map_err(|e| LiveError::BadOrder(format!("bad assoc_curve '{}': {e}", position.assoc_curve)))?,
                amount_tokens: position.amount_tokens,
                min_sol_output: floor,
                tip_lamports: self.tip_lamports,
                creator: match &position.creator {
                    Some(c) => Some(
                        parse_pubkey(c)
                            .map_err(|e| LiveError::BadOrder(format!("bad creator '{c}': {e}")))?,
                    ),
                    None => None,
                },
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
        match self
            .executor
            .confirm_signature(&receipt.signature, LIVE_CONFIRM_TIMEOUT)
            .await
        {
            Ok(ConfirmOutcome::Confirmed) => {
                // Write-ahead of the pipeline's custody subtract: a crash
                // in between replays to a subtracted position, never to a
                // resurrected bag (which would double-sell on retry).
                self.record_sell(&position.mint, position.amount_tokens, &receipt.bundle_id);
                Ok(receipt.bundle_id)
            }
            Ok(ConfirmOutcome::FailedOnChain) => Err(LiveError::Unconfirmed(format!(
                "sell bundle {} errored on-chain; still holding",
                receipt.bundle_id
            ))),
            Ok(ConfirmOutcome::Timeout) => Err(LiveError::Unconfirmed(format!(
                "sell bundle {} unconfirmed after {:?}; still holding",
                receipt.bundle_id, LIVE_CONFIRM_TIMEOUT
            ))),
            // Transport failure mid-confirm: the bundle may already have
            // landed, so report it as unconfirmed-but-holding (same retry
            // path as a timeout) rather than a dispatch error.
            Err(e) => Err(LiveError::Unconfirmed(format!(
                "sell bundle {} confirm RPC failed ({e}); still holding",
                receipt.bundle_id
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slippage_ceiling_covers_fee_inclusive_quotes() {
        // The frozen 6002 log: quote required 1_668_970, guard sent
        // 1_650_001 (1.15% short). At 100bps the guard must be
        // ceil(1_668_970 * 1.01) = 1_685_660 — above `Right`, never
        // equal-or-below it.
        assert_eq!(apply_slippage_ceiling(1_668_970, 100), Some(1_685_660));
        // Ceiling, not floor: 1 lamport of fraction rounds the guard up.
        assert_eq!(apply_slippage_ceiling(10_001, 0), Some(10_001));
        assert_eq!(apply_slippage_ceiling(10_000, 1), Some(10_001));
        // Degenerate quotes fail closed; bps clamps at 100%.
        assert_eq!(apply_slippage_ceiling(0, 200), None);
        assert_eq!(apply_slippage_ceiling(1_000, 10_000), Some(2_000));
        assert_eq!(apply_slippage_ceiling(1_000, 99_999), Some(2_000));
        // Overflow fails closed instead of saturating into a jumbo guard.
        assert_eq!(apply_slippage_ceiling(u64::MAX, 10_000), None);
        assert_eq!(apply_slippage_ceiling(u64::MAX, 0), Some(u64::MAX));
    }

    #[test]
    fn buy_plan_math() {
        // $10 at $0.0000042 into $30k liquidity: the paper fill model
        // takes fee + impact first, so live sizes ~2.3508M tokens
        // (2_350_783_986_459 base units) — identical inputs size
        // identically on both legs.
        let plan = plan_buy(10.0, 0.000_004_2, 150.0, 200, 30_000.0).expect("plan");
        assert_eq!(plan.amount_tokens, 2_350_783_986_459);
        // $10 ≈ 66.67M lamports + 1.25% venue fee, then + 2% slippage guard.
        assert_eq!(plan.max_sol_cost, 68_850_000);
    }

    #[test]
    fn buy_plan_rejects_degenerate() {
        assert_eq!(plan_buy(0.0, 1.0, 150.0, 200, 30_000.0), None);
        assert_eq!(plan_buy(10.0, 0.0, 150.0, 200, 30_000.0), None);
        assert_eq!(plan_buy(10.0, 1.0, 0.0, 200, 30_000.0), None);
        assert_eq!(plan_buy(f64::NAN, 1.0, 150.0, 200, 30_000.0), None);
        // Dust that rounds to zero units.
        assert_eq!(plan_buy(1e-12, 1.0, 150.0, 200, 30_000.0), None);
    }

    #[test]
    fn sell_floor_math() {
        // 1e6 units (1 token) at $2, SOL $150: model proceeds land
        // below quote (fee + impact), then − 2% margin → 12_902_462.
        let floor = plan_sell_floor(1_000_000, 2.0, 150.0, 200, 30_000.0).expect("floor");
        assert_eq!(floor, 12_902_462);
    }

    #[test]
    fn buy_plan_rejects_overflow() {
        // 1e36 base units does not fit u64: fail closed, never saturate.
        assert_eq!(plan_buy(1e20, 1e-10, 150.0, 200, 30_000.0), None);
        // Oversized spend cap (amount fits, cap does not): same treatment.
        assert_eq!(plan_buy(1e13, 1e6, 150.0, 200, 30_000.0), None);
    }

    #[test]
    fn sell_floor_rejects_zero_floor() {
        // 100% slippage authorizes any fill price: rejected outright.
        assert_eq!(plan_sell_floor(1_000_000, 2.0, 150.0, 10_000, 30_000.0), None);
        assert_eq!(plan_sell_floor(1_000_000, 2.0, 150.0, 10_001, 30_000.0), None);
        // Sub-lamport dust floors to 0: no unprotected dispatch.
        assert_eq!(plan_sell_floor(1, 1e-9, 1e9, 200, 30_000.0), None);
    }

    #[test]
    fn sell_floor_rejects_degenerate() {
        assert_eq!(plan_sell_floor(0, 2.0, 150.0, 200, 30_000.0), None);
        assert_eq!(plan_sell_floor(100, 0.0, 150.0, 200, 30_000.0), None);
        // Absurd proceeds that don't fit u64: fail closed, never saturate.
        assert_eq!(plan_sell_floor(u64::MAX, 1e12, 1e-6, 0, 30_000.0), None);
    }

    #[test]
    fn live_sizes_match_paper_ledger() {
        // Parity lock: identical inputs must size identically on both
        // legs (paper fills at quote*(1+slip); live sizes from the same
        // filled price, floored to base units). Commission stays 0 here
        // to isolate the fill model — live dispatch costs ride the
        // day-loss breaker, not the sizing math.
        use simulator::{PaperPortfolio, Side};
        let mut paper = PaperPortfolio::new(1_000.0, 0.0);
        let id = paper
            .open_position(
                "MINT", Side::Long, 0.000_004_2, 10.0, 30_000.0, 75.0, -8.0, false, 0.0,
                0, 0.0, 1_000,
            )
            .expect("open");
        let qty = paper.position(id).expect("pos").quantity;
        let plan = plan_buy(10.0, 0.000_004_2, 150.0, 200, 30_000.0).expect("plan");
        // Paper quantity is whole tokens; live tracks base units (1e6).
        assert_eq!(plan.amount_tokens, (qty * 1e6).floor() as u64);
        // Sell floor sits below modeled gross proceeds (conservative by
        // construction: model slip first, then the slippage margin).
        // One token at $3 into $30k liquidity: slip ≈ fee-only, so the
        // floor lands just under gross (the full-size plan above would
        // pin the 25% impact cap instead — same model, both legs).
        let floor = plan_sell_floor(1_000_000, 3.0, 150.0, 200, 30_000.0).expect("floor");
        let gross = 1_000_000_f64 / 1e6 * 3.0 / 150.0 * LAMPORTS_PER_SOL;
        assert!((floor as f64) < gross, "floor {floor} vs gross {gross}");
        assert!((floor as f64) > gross * 0.9, "floor {floor} vs gross {gross}");
    }

    #[test]
    fn wal_replay_folds_pending_settled_positions_sells() {
        // Ordered fold: pending survives, settled retires, sells subtract,
        // manual restores, corrupt lines count (never panic).
        let lines = [
            r#"{"t":"pending","mint":"MINT","curve":"CURVE","assoc":"ASSOC","amount":1000,"creator":null,"sig":"SIG1","bundle":"B1","ts":1}"#,
            r#"{"t":"pending","mint":"DEAD","curve":"CURVE","assoc":"ASSOC","amount":500,"creator":null,"sig":"SIG2","bundle":"B2","ts":2}"#,
            r#"{"t":"settled","bundle":"B2","ts":3}"#,
            r#"{"t":"position","mint":"HELD","curve":"CURVE","assoc":"ASSOC","amount":700,"creator":null,"bundle":"B3","ts":4}"#,
            r#"{"t":"sell","mint":"HELD","amount":200,"bundle":"B4","ts":5}"#,
            r#"{"t":"manual","mint":"STUCK","curve":"CURVE","assoc":"ASSOC","amount":300,"creator":null,"ts":6}"#,
            "not json at all",
            r#"{"t":"pending","mint":"BROKEN","ts":7}"#,
        ];
        let replay = wal::replay_lines(lines.into_iter());
        assert_eq!(replay.pending.len(), 1);
        assert_eq!(replay.pending[0].bundle_id, "B1");
        assert_eq!(replay.pending[0].signature, "SIG1");
        assert_eq!(replay.positions.len(), 1);
        assert_eq!(replay.positions[0].mint, "HELD");
        assert_eq!(replay.positions[0].amount_tokens, 500);
        assert_eq!(replay.manual.len(), 1);
        assert_eq!(replay.manual[0].position.mint, "STUCK");
        assert_eq!(replay.manual[0].note, "");
        assert_eq!(replay.skipped, 2);
    }
}

//! Offline replay + paper/live metric separation.
//!
//! `pump-sentry replay` re-derives the paper ledger's recorded
//! decisions from ONLY the inputs in the decision journal
//! (`journal::JournalLine`): sequential ticks for held mints drive the
//! same `PaperPortfolio` path the runtime used, every recorded exit
//! must match the re-derived trigger, and every recorded close must
//! match the re-derived trade numbers. Missing input lines (a dropped
//! tick, an unreadable line) make the decision UNVERIFIABLE and fail
//! the exit code — replay never fills a gap by assuming.
//!
//! Metrics stay in two strict buckets. Paper numbers describe the
//! SIMULATOR (model fills, booked fees, calm-weather dispatch legs);
//! live numbers describe chain outcomes (attempts, confirmations,
//! parked exits, settled gaps, real costs). Paper win rate is never
//! presented as evidence of live profitability — the report says so,
//! next to the numbers.

use std::collections::HashMap;
use std::path::PathBuf;

use simulator::{ExitReason, PaperPortfolio};

use crate::journal::{self, JournalLine};

/// The decision journal for offline replay; human report, then exit
/// code 0 (fully reproduced) or 1 (any mismatch or unverifiable
/// decision). `None` file uses the default journal next to the log.
pub async fn cmd_replay(file: Option<String>) -> ! {
    let path: PathBuf = file
        .map(PathBuf::from)
        .unwrap_or_else(journal::default_journal_path);
    let (lines, blank, bad) = journal::load_lines(&path);
    println!(
        "journal: {} ({} recorded, {} blank, {} skipped-bad lines)",
        path.display(),
        lines.len(),
        blank,
        bad
    );
    if lines.is_empty() {
        println!("nothing to replay: the bot has not recorded any decisions yet");
        std::process::exit(0);
    }
    let verify = verify(&lines);
    print_verify(&verify);
    print_metrics(&metrics(&lines));
    if verify.mismatches == 0 && verify.unverified == 0 {
        println!("VERDICT: PASS (replay reproduced every recorded trigger and close)");
        std::process::exit(0);
    }
    let mis = verify.mismatches;
    let unv = verify.unverified;
    println!("VERDICT: FAIL ({mis} mismatched, {unv} unverifiable — see details above)");
    std::process::exit(1);
}

/// Re-derivation outcome, cumulative over the whole journal.
#[derive(Debug, Default)]
pub struct VerifyReport {
    pub boots: usize,
    pub opens_ok: usize,
    pub open_mismatches: usize,
    pub triggers_ok: usize,
    pub trigger_mismatches: usize,
    pub triggers_unverified: usize,
    pub closes_ok: usize,
    pub close_mismatches: usize,
    /// Human-legible mismatch details, oldest first (bounded).
    pub details: Vec<String>,
    /// Non-fatal observations (settings drift without restart); never
    /// counted as mismatch/unverified.
    pub notes: Vec<String>,
    pub mismatches: usize,
    pub unverified: usize,
}

impl VerifyReport {
    fn mismatch(&mut self, what: String) {
        self.mismatches += 1;
        self.detail(what);
    }
    fn unverified(&mut self, what: String) {
        self.unverified += 1;
        self.detail(what);
    }
    fn note(&mut self, what: String) {
        self.notes.push(what);
        if self.notes.len() > 20 {
            self.notes.truncate(20);
        }
    }
    fn detail(&mut self, what: String) {
        self.details.push(what);
        if self.details.len() > 20 {
            self.details.truncate(20);
        }
    }
}

/// Paper vs live metric buckets (kept apart, always).
#[derive(Debug, Default)]
pub struct Metrics {
    // ---- paper (simulator world) ----
    paper_trades: usize,
    paper_wins: usize,
    paper_net_pnl: f64,
    paper_fees: f64,
    paper_dispatch_bleed: f64,
    paper_hold_secs: u64,
    paper_drawdown_abs: f64,
    paper_drawdown_pct: f64,
    paper_equity_samples: usize,
    // ---- live (chain world) ----
    live_buys_dispatched: usize,
    live_buys_skipped: usize,
    live_buys_confirmed: usize,
    live_buys_dead: usize,
    live_buys_giveup: usize,
    live_buys_unresolved: usize,
    live_sells_dispatched: usize,
    live_sell_giveups: usize,
    entry_gaps: usize,
    entry_gap_total_usd: f64,
    exit_gaps: usize,
    exit_gap_total_usd: f64,
    /// Per-venue gap tables `(count, total)`: which dispatch path bled.
    /// Pre-dimension lines (empty venue) read as "unknown" so the rows
    /// always sum back to the leg totals above — never silent drops.
    entry_gap_by_venue: HashMap<String, (usize, f64)>,
    exit_gap_by_venue: HashMap<String, (usize, f64)>,
    live_cost_usd: f64,
    /// Live estimate: paper edges (live mode) + settled exit gaps −
    /// UNPAIRED dispatch debits. Paired tips ride inside their gap.
    live_net_estimate: f64,
}

fn line_debit_usd(line: &JournalLine) -> Option<f64> {
    match line {
        JournalLine::BuyAttempt { debit_usd, .. }
        | JournalLine::BuyOutcome { debit_usd, .. }
        | JournalLine::SellAttempt { debit_usd, .. } => *debit_usd,
        _ => None,
    }
}

/// Re-derive the recorded decisions. Uses the same public portfolio
/// calls the runtime used (`open_position_params`,
/// `check_exit_realizable`, `close_partial_position`), fed with
/// recorded marks only — no future information exists in the stream.
/// A restart mid-run (a second `Boot`) resets the reconstructed book;
/// recorded positions before it were the closed book's business.
pub fn verify(lines: &[JournalLine]) -> VerifyReport {
    let mut report = VerifyReport::default();
    let mut book: Option<PaperPortfolio> = None;
    let mut mode: &str = "";
    let mut settings = journal::BootSettings::pre_boot();
    // recorded id → local book id and back. Both are deterministic
    // counters, so they agree when reconstruction is faithful; the
    // map keeps a lost-open-record case loud instead of mis-closing.
    let mut record_to_local: HashMap<u64, u64> = HashMap::new();
    let mut local_to_record: HashMap<u64, u64> = HashMap::new();
    // Trigger result per recorded id, from the latest evaluated tick.
    let mut last_check: HashMap<u64, Option<ExitReason>> = HashMap::new();
    // The mark of the latest evaluated tick per mint: an exit line
    // must ride exactly this mark, or its causal input is missing.
    let mut last_tick_mark: HashMap<String, simulator::Mark> = HashMap::new();

    for line in lines {
        match line {
            JournalLine::Boot { settings: s, .. } => {
                report.boots += 1;
                mode = s.mode.as_str();
                settings = s.clone();
                book = Some(PaperPortfolio::new(s.start_balance, s.fee_bps));
                record_to_local.clear();
                local_to_record.clear();
                last_check.clear();
                last_tick_mark.clear();
            }
            JournalLine::Tick {
                mint,
                mark,
                tick_observed_at,
                complete,
                manual_exits,
                sol_feed,
                ..
            } => {
                let Some(book) = book.as_mut() else { continue };
                // Paper-only dispatch legs re-price from the recorded
                // feed at each tick — identical to the runtime path.
                if mode == "paper"
                    && let Some(sol) = sol_feed
                {
                    apply_paper_legs(book, *sol, &settings);
                }
                for local in book.positions_for_mint(mint) {
                    let reason = if *complete || *manual_exits {
                        // Migration closes AND operator sells bypass the
                        // trigger check: the runtime booked them Manual
                        // without one, so re-derivation must too (a
                        // market tick never forces Manual — the flag
                        // stays false there).
                        Some(ExitReason::Manual)
                    } else {
                        book.check_exit_realizable(local, mark, *tick_observed_at)
                    };
                    if let Some(&recorded) = local_to_record.get(&local) {
                        last_check.insert(recorded, reason);
                    }
                }
                last_tick_mark.insert(mint.clone(), *mark);
            }
            JournalLine::Open {
                position,
                venue_fee_bps,
                sol_feed,
                mark,
                ticket_usd,
                ..
            } => {
                let Some(book) = book.as_mut() else {
                    report.mismatch(format!(
                        "open {} #{}: boot record missing",
                        position.mint, position.id
                    ));
                    continue;
                };
                if mode == "paper"
                    && let Some(sol) = sol_feed
                {
                    apply_paper_legs(book, *sol, &settings);
                }
                book.set_venue_fee_for_mint(&position.mint, *venue_fee_bps);
                // Replay sizes on the RECORDED ticket (single fact, no
                // inference); a delta from the Boot snapshot means the
                // operator changed settings without a restart — that is
                // a NOTE, never a numeric mismatch cascade.
                let ticket = if *ticket_usd > 0.0 {
                    *ticket_usd
                } else {
                    // Pre-upgrade lines without the field fall back to
                    // the boot snapshot (previous behavior).
                    settings.notional_usd
                };
                if *ticket_usd > 0.0 && *ticket_usd != settings.notional_usd {
                    report.note(format!(
                        "notional scaled {} at open {} #{} vs boot snapshot {} (settings changed without restart)",
                        ticket_usd, position.mint, position.id, settings.notional_usd
                    ));
                }
                let opened = book.open_position_params(&simulator::OpenPositionParams {
                    mint: &position.mint,
                    side: position.side,
                    mark: *mark,
                    notional_usd: ticket,
                    tp_percent: position.tp_percent,
                    sl_percent: position.sl_percent,
                    trailing: position.trailing_stop,
                    trail_percent: position.trail_percent,
                    timeout_seconds: position.timeout_seconds,
                    moonbag_percent: position.moonbag_percent,
                    now_secs: position.opened_at,
                });
                match opened {
                    Ok(local) => {
                        if position_matches(recorded_position(book, local), position) {
                            record_to_local.insert(position.id, local);
                            local_to_record.insert(local, position.id);
                            report.opens_ok += 1;
                        } else {
                            report.open_mismatches += 1;
                            let booked = position;
                            report.mismatch(format!(
                                "open {} #{}: re-derived numbers differ (booked entry {:.10} qty {} fee {:.10} vs {booked:?})",
                                position.mint,
                                position.id,
                                booked.entry_price,
                                booked.quantity,
                                booked.entry_fee,
                            ));
                        }
                    }
                    Err(e) => {
                        report.open_mismatches += 1;
                        report.mismatch(format!(
                            "open {} #{}: re-derived open refused: {e}",
                            position.mint, position.id
                        ));
                    }
                }
            }
            JournalLine::Exit {
                mint,
                trade,
                fraction,
                mark,
                tick_observed_at,
                ..
            } => {
                let Some(book) = book.as_mut() else {
                    report.mismatch(format!("exit {} #{}: boot record missing", mint, trade.id));
                    continue;
                };
                let Some(&local) = record_to_local.get(&trade.id) else {
                    report.mismatch(format!(
                        "exit {} #{}: open record missing — cannot claim this exit",
                        mint, trade.id
                    ));
                    continue;
                };
                // A missing causal input is UNVERIFIABLE, never
                // assumed: if the exit did not ride the last tick the
                // journal has for this mint, the tick line (or the
                // exit's mark) is not what produced it.
                let input_missing = last_tick_mark
                    .get(mint)
                    .is_some_and(|last| !mark_same(last, mark));
                match last_check.get(&trade.id) {
                    Some(expected) if !input_missing => {
                        if *expected == Some(trade.reason) {
                            report.triggers_ok += 1;
                        } else {
                            report.trigger_mismatches += 1;
                            report.mismatch(format!(
                                "exit {} #{}: recorded reason {:?}, re-derivation says {:?}",
                                mint, trade.id, trade.reason, expected
                            ));
                        }
                    }
                    _ => {
                        report.triggers_unverified += 1;
                        report.unverified(format!(
                            "exit {} #{}: tick record missing before this exit — unverifiable",
                            mint, trade.id
                        ));
                    }
                }
                let expect_frac =
                    simulator::market::exit_fraction(trade.reason, book.moonbag_of(local));
                if !close_f64(*fraction, expect_frac) {
                    report.mismatch(format!(
                        "exit {} #{}: fraction {fraction} differs from re-derivation {expect_frac}",
                        mint, trade.id
                    ));
                }
                match book.close_partial_position(
                    local,
                    *fraction,
                    mark,
                    trade.reason,
                    *tick_observed_at,
                ) {
                    Ok(replayed) => {
                        if trade_matches(trade, &replayed, local) {
                            report.closes_ok += 1;
                        } else {
                            report.close_mismatches += 1;
                            report.mismatch(format!(
                                "exit {} #{}: re-derived trade numbers differ (net/fees/exit price)",
                                mint, trade.id
                            ));
                        }
                    }
                    Err(e) => {
                        report.close_mismatches += 1;
                        report.mismatch(format!(
                            "exit {} #{}: re-derived close refused: {e}",
                            mint, trade.id
                        ));
                    }
                }
                last_check.remove(&trade.id);
            }
            JournalLine::BuyAttempt { .. }
            | JournalLine::BuyOutcome { .. }
            | JournalLine::SellAttempt { .. } => {
                // Live debits reconstruct cash exactly as the runtime
                // debited it, so later opens re-derive with the same
                // balance checks.
                if mode == "live"
                    && let Some(debit) = line_debit_usd(line)
                    && let Some(book) = book.as_mut()
                {
                    book.debit_dispatch_cost(debit);
                }
            }
            _ => {}
        }
    }
    report
}

/// Recorded-marks identity: identical bit shapes (recorded floats come
/// from the same serialization path, so exactness is meaningful here).
fn mark_same(a: &simulator::Mark, b: &simulator::Mark) -> bool {
    a.price == b.price && a.liquidity_usd == b.liquidity_usd && a.curve == b.curve
}

fn recorded_position(book: &PaperPortfolio, local: u64) -> Option<&simulator::Position> {
    book.position(local)
}

fn apply_paper_legs(book: &mut PaperPortfolio, sol_usd: f64, s: &journal::BootSettings) {
    if s.tip_lamports == 0 || !sol_usd.is_finite() || sol_usd <= 0.0 {
        return;
    }
    let (open_leg, close_leg) = crate::live::paper_dispatch_legs_usd(
        s.tip_lamports,
        crate::live::priority_fee_lamports(s.compute_unit_limit, s.priority_fee_micro_lamports),
        sol_usd,
    );
    book.set_paper_dispatch_legs(open_leg, close_leg);
}

/// Tolerance for re-derived floats: identical code + identical inputs
/// must be identical; beyond 1e-9 needs a human-eyed mismatch report.
fn close_f64(a: f64, b: f64) -> bool {
    if a.is_nan() && b.is_nan() {
        return true;
    }
    (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0)
}

fn position_matches(
    replayed: Option<&simulator::Position>,
    recorded: &simulator::Position,
) -> bool {
    let Some(r) = replayed else { return false };
    recorded.quantity == r.quantity
        && recorded.side == r.side
        && close_f64(recorded.entry_price, r.entry_price)
        && close_f64(recorded.entry_notional, r.entry_notional)
        && close_f64(recorded.entry_fee, r.entry_fee)
}

fn trade_matches(
    recorded: &simulator::ClosedTrade,
    replayed: &simulator::ClosedTrade,
    local: u64,
) -> bool {
    replayed.id == local
        && recorded.quantity == replayed.quantity
        && close_f64(recorded.exit_price, replayed.exit_price)
        && close_f64(recorded.fees, replayed.fees)
        && close_f64(recorded.net_pnl, replayed.net_pnl)
        && close_f64(recorded.dispatch_usd, replayed.dispatch_usd)
}

/// Aggregate metrics from the same lines, strictly two buckets.
pub fn metrics(lines: &[JournalLine]) -> Metrics {
    let mut m = Metrics::default();
    // Paper edges seed the live estimate ONLY in live mode: there every
    // live-held mint mirrors a paper close. Paper-only keys (no live
    // leg ever dispatched) would inflate a live estimate they were
    // never live on.
    let mut mode = "";
    let mut equity_peak: Option<f64> = None;
    let mut realized_cum: f64 = 0.0;
    let mut realized_peak: f64 = 0.0;
    for line in lines {
        match line {
            JournalLine::Boot { settings: s, .. } => {
                mode = s.mode.as_str();
                // A restart restarts the realized-cum drawdown series;
                // heartbeats (equity samples) cover trading across
                // boots.
                realized_cum = 0.0;
                realized_peak = 0.0;
            }
            JournalLine::Exit { trade, .. } => {
                m.paper_trades += 1;
                m.paper_net_pnl += trade.net_pnl;
                m.paper_fees += trade.fees;
                m.paper_dispatch_bleed += trade.dispatch_usd;
                m.paper_hold_secs += trade.hold_seconds;
                if trade.net_pnl > 0.0 {
                    m.paper_wins += 1;
                }
                realized_cum += trade.net_pnl - trade.dispatch_usd;
                realized_peak = realized_peak.max(realized_cum);
                m.paper_drawdown_abs = m.paper_drawdown_abs.max(realized_peak - realized_cum);
                if mode == "live" {
                    m.live_net_estimate += trade.net_pnl;
                }
            }
            JournalLine::Equity { equity, .. } => {
                m.paper_equity_samples += 1;
                let peak = equity_peak.map_or(*equity, |p| p.max(*equity));
                equity_peak = Some(peak);
                let dd_abs = peak - equity;
                if dd_abs > m.paper_drawdown_abs {
                    m.paper_drawdown_abs = dd_abs;
                    m.paper_drawdown_pct = if peak > 0.0 {
                        dd_abs / peak * 100.0
                    } else {
                        0.0
                    };
                }
            }
            JournalLine::BuyAttempt { debit_usd, .. } => {
                m.live_buys_dispatched += 1;
                debit_cost(&mut m, *debit_usd, true);
            }
            JournalLine::BuySkip { .. } => m.live_buys_skipped += 1,
            JournalLine::BuyOutcome {
                verdict, debit_usd, ..
            } => {
                match verdict.as_str() {
                    "confirmed" => m.live_buys_confirmed += 1,
                    "dead" => m.live_buys_dead += 1,
                    "giveup" => m.live_buys_giveup += 1,
                    _ => m.live_buys_unresolved += 1,
                }
                debit_cost(&mut m, *debit_usd, true);
            }
            JournalLine::SellAttempt {
                debit_usd,
                settle_paired,
                ..
            } => {
                m.live_sells_dispatched += 1;
                // A paired settle's tip is already inside its exit Gap
                // line — subtracting here too would count it twice.
                debit_cost(&mut m, *debit_usd, !*settle_paired);
            }
            JournalLine::SellGiveUp { .. } => m.live_sell_giveups += 1,
            JournalLine::Gap {
                leg,
                gap_usd,
                venue,
                ..
            } => {
                // Pre-dimension lines carry no venue: "unknown" keeps
                // the rows summing back to the leg totals.
                let venue = if venue.is_empty() { "unknown" } else { venue };
                if leg == "entry" {
                    m.entry_gaps += 1;
                    m.entry_gap_total_usd += gap_usd;
                    let e = m
                        .entry_gap_by_venue
                        .entry(venue.to_string())
                        .or_insert((0, 0.0));
                    e.0 += 1;
                    e.1 += gap_usd;
                } else {
                    m.exit_gaps += 1;
                    m.exit_gap_total_usd += gap_usd;
                    let e = m
                        .exit_gap_by_venue
                        .entry(venue.to_string())
                        .or_insert((0, 0.0));
                    e.0 += 1;
                    e.1 += gap_usd;
                    // The exit gap pairs the recorded paper close:
                    // live realized = paper edge + settled gap
                    // (exit tip inside the gap).
                    m.live_net_estimate += gap_usd;
                }
            }
            _ => {}
        }
    }
    m
}

fn debit_cost(m: &mut Metrics, debit_usd: Option<f64>, subtract: bool) {
    if let Some(d) = debit_usd {
        m.live_cost_usd += d;
        // Costs always show up in `committed costs`; the estimate only
        // subtracts debits not already owned elsewhere: buy-side
        // debits and UNPAIRED sell debits go here explicitly, while a
        // paired sell's tip rides inside its exit Gap line (the
        // tracker is the single owner of that math).
        if subtract {
            m.live_net_estimate -= d;
        }
    }
}

fn print_verify(v: &VerifyReport) {
    println!("-- replay verification (decisions re-derived from recorded inputs only) --");
    println!(
        "boots {} | opens {}/{} reproduced | triggers {}/{} reproduced, {} unverifiable | closes {}/{} numbers match",
        v.boots,
        v.opens_ok,
        v.opens_ok + v.open_mismatches,
        v.triggers_ok,
        v.triggers_ok + v.trigger_mismatches,
        v.triggers_unverified,
        v.closes_ok,
        v.closes_ok + v.close_mismatches,
    );
    for d in &v.details {
        println!("  {d}");
    }
    for n in &v.notes {
        println!("  note: {n}");
    }
}

fn print_metrics(m: &Metrics) {
    println!("-- paper (simulator: model fills, booked fees; NOT live evidence) --");
    let denom = m.paper_trades.max(1);
    println!(
        "trades {} | wins {} ({:.1}%) | net PnL {:+.4} USD (edge, net incl. bleed {:+.4})",
        m.paper_trades,
        m.paper_wins,
        m.paper_wins as f64 / denom as f64 * 100.0,
        m.paper_net_pnl,
        m.paper_net_pnl - m.paper_dispatch_bleed,
    );
    println!(
        "expectancy {:+.4} USD/trade (incl. dispatch bleed) | avg cost {:.4}/trade (fees {:.4} + bleed {:.4}) | avg hold {:.0}s",
        (m.paper_net_pnl - m.paper_dispatch_bleed) / denom as f64,
        (m.paper_fees + m.paper_dispatch_bleed) / denom as f64,
        m.paper_fees,
        m.paper_dispatch_bleed,
        m.paper_hold_secs as f64 / denom as f64,
    );
    if m.paper_equity_samples > 0 {
        println!(
            "max drawdown {:.4} USD ({:.1}% from peak, {} equity samples)",
            m.paper_drawdown_abs, m.paper_drawdown_pct, m.paper_equity_samples,
        );
    } else {
        println!(
            "max drawdown {:.4} USD (realized series; no equity heartbeats recorded)",
            m.paper_drawdown_abs,
        );
    }
    println!();
    println!("-- live (chain outcomes; separate reality from paper) --");
    let finalized_buys = m.live_buys_confirmed + m.live_buys_dead + m.live_buys_giveup;
    let fill_rate = if finalized_buys == 0 {
        "n/a".to_string()
    } else {
        format!(
            "{:.1}%",
            m.live_buys_confirmed as f64 / finalized_buys as f64 * 100.0
        )
    };
    println!(
        "buys dispatched {} (skipped {}): confirmed {} | dead {} | giveup/parked {} | unresolved {} | fill rate (of finalized) {fill_rate}",
        m.live_buys_dispatched,
        m.live_buys_skipped,
        m.live_buys_confirmed,
        m.live_buys_dead,
        m.live_buys_giveup,
        m.live_buys_unresolved,
    );
    let failed_exit_rate = if m.live_sells_dispatched == 0 {
        "n/a".to_string()
    } else {
        format!(
            "{:.1}%",
            m.live_sell_giveups as f64 / m.live_sells_dispatched as f64 * 100.0
        )
    };
    println!(
        "sells dispatched {} | parked/gave up {} | failed-exit rate (of dispatched) {failed_exit_rate}",
        m.live_sells_dispatched, m.live_sell_giveups,
    );
    println!(
        "execution gap: entry n={} total {:+.4} USD | exit n={} total {:+.4} USD (lower bound: tick proxy, excl. intra-bundle slip; see FillGap docs)",
        m.entry_gaps, m.entry_gap_total_usd, m.exit_gaps, m.exit_gap_total_usd,
    );
    print_gap_venues("entry", &m.entry_gap_by_venue);
    print_gap_venues("exit", &m.exit_gap_by_venue);
    println!(
        "committed costs {:+.4} USD (recorded debits at actuals) | live net PnL ESTIMATE {:+.4} USD (paper edge + settled exit gaps − unpaired dispatch debits)",
        m.live_cost_usd, m.live_net_estimate,
    );
    println!();
    println!(
        "Paper numbers describe the simulator only. Paper win rate is NOT evidence that live trades profit — judge live profitability by the live bucket alone."
    );
}

fn print_gap_venues(leg: &str, table: &HashMap<String, (usize, f64)>) {
    let mut rows: Vec<(&String, &(usize, f64))> = table.iter().collect();
    rows.sort_by(|a, b| a.0.cmp(b.0));
    for (venue, (n, total)) in rows {
        println!("  {leg} gap venue {venue}: n={n} total {total:+.4} USD");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::{Decoded, decode_line};
    use simulator::{Mark, OpenPositionParams, PaperPortfolio, Side};

    /// Build a fixture with `boot(..)`; deterministic, no curve state
    /// (proxy fills, finite liquidity everywhere).
    fn boot(_ts: u64, mode: &str) -> journal::BootSettings {
        journal::BootSettings {
            mode: mode.to_string(),
            strategy_mode: "adaptive".into(),
            start_balance: 200.0,
            fee_bps: 100.0,
            notional_usd: 100.0,
            trail_percent: 25.0,
            runner_trail_percent: 25.0,
            runner_moonbag_percent: 0.0,
            slippage_bps: 250,
            tip_lamports: 0,
            priority_fee_micro_lamports: 0,
            compute_unit_limit: 180_000,
            max_open_positions: 3,
            max_exposure_usd: 100.0,
            max_daily_loss_usd: 50.0,
            hot_wallet_max_sol: 0.0,
            mint_cooldown_secs: 120,
            gate_min_buyers: 3,
            gate_window_secs: 10,
            inference_endpoint: String::new(),
            inference_model: String::new(),
            inference_timeout_ms: 0,
            version: "0".into(),
        }
    }

    fn outmark(price: f64) -> Mark {
        Mark::new(price, 50_000.0)
    }

    /// Build a full lifecycle: open at 1.0, TP at 1.30 (mark +30%).
    /// Returns the journal lines AND the paper's realized PnL for
    /// cross-checks. Standard fixture: ticket = boot notional (100),
    /// paper mode.
    fn lifecycle() -> (Vec<JournalLine>, f64) {
        lifecycle_mode("paper", 100.0)
    }

    /// Fixture with explicit mode/ticket: in production the gate passes
    /// `settings.notional_usd` straight through the open; a test can
    /// drift it from the Boot snapshot to pin the NOTE behavior.
    fn lifecycle_mode(mode: &str, ticket: f64) -> (Vec<JournalLine>, f64) {
        let mut settings = boot(1_000, mode);
        settings.tip_lamports = 1_000;
        let mut book = PaperPortfolio::new(settings.start_balance, settings.fee_bps);
        let mut journal: Vec<JournalLine> = Vec::new();
        journal.push(JournalLine::Boot {
            ts: 1_000,
            settings: settings.clone(),
        });

        // Open: the runtime seeds venue fee + legs from the feed.
        let venue_bps = 125.0f64;
        let sol_feed = 150.0f64;
        book.set_venue_fee_for_mint("M", venue_bps);
        apply_paper_legs(&mut book, sol_feed, &settings);
        let id = book
            .open_position_params(&OpenPositionParams {
                mint: "M",
                side: Side::Long,
                mark: outmark(1.0),
                notional_usd: ticket,
                tp_percent: 8.0,
                sl_percent: -20.0,
                trailing: false,
                trail_percent: settings.trail_percent,
                timeout_seconds: 300,
                moonbag_percent: 0.0,
                now_secs: 1_001,
            })
            .expect("fixture open");
        let position = book.position(id).unwrap().clone();
        journal.push(JournalLine::Open {
            ts: 1_001,
            mint: position.mint.clone(),
            risk: "model".into(),
            venue_fee_bps: venue_bps,
            sol_feed: Some(sol_feed),
            mark: outmark(1.0),
            ticket_usd: ticket,
            position: position.clone(),
        });

        // benign tick, then the TP tick. Tick lines first (runtime
        // order), then the exit re-derived via the book itself.
        let now = 1_010;
        journal.push(JournalLine::Tick {
            ts: now,
            mint: "M".into(),
            mark: outmark(1.02),
            sol_feed: Some(sol_feed),
            tick_observed_at: now,
            complete: false,
            manual_exits: false,
        });
        let tnow = 1_020;
        let tp_mark = outmark(1.30);
        journal.push(JournalLine::Tick {
            ts: tnow,
            mint: "M".into(),
            mark: tp_mark,
            sol_feed: Some(sol_feed),
            tick_observed_at: tnow,
            complete: false,
            manual_exits: false,
        });
        let reason = book
            .check_exit_realizable(id, &tp_mark, tnow)
            .expect("fixture TP fires at +30%");
        assert_eq!(reason, ExitReason::TakeProfit);
        let trade = book
            .close_partial_position(id, 1.0, &tp_mark, reason, tnow)
            .expect("fixture close");
        let realized = trade.net_pnl;
        journal.push(JournalLine::Exit {
            ts: tnow,
            mint: "M".into(),
            fraction: 1.0,
            tick_observed_at: tnow,
            trade: trade.clone(),
            mark: tp_mark,
        });
        (journal, realized)
    }

    #[test]
    fn replay_reproduces_recorded_lifecycle() {
        let (lines, realized) = lifecycle();
        let report = verify(&lines);
        assert_eq!(report.boots, 1);
        assert_eq!(report.opens_ok, 1, "open re-derivation: {report:?}");
        assert_eq!(report.opens_ok + report.open_mismatches, 1);
        assert_eq!(report.triggers_ok, 1, "trigger: {report:?}");
        assert_eq!(report.closes_ok, 1, "close: {report:?}");
        assert_eq!(report.mismatches, 0);
        assert_eq!(report.unverified, 0);
        assert!(report.notes.is_empty(), "{report:?}");

        let m = metrics(&lines);
        assert_eq!(m.paper_trades, 1);
        assert_eq!(m.paper_wins, 1);
        assert!((m.paper_net_pnl - realized).abs() < 1e-12);
        // no equity heartbeats: drawdown from the realized series only
        assert_eq!(m.paper_equity_samples, 0);
        assert!(m.paper_drawdown_abs.abs() < 1e-12);
    }

    /// Ticket size is RECORDED per open: a mid-run settings change must
    /// reproduce the open exactly (it is a note, not a mismatch
    /// cascade taking its exits down with it).
    #[test]
    fn replay_notes_midrun_notional_drift_instead_of_mismatch() {
        let (lines, _) = lifecycle_mode("paper", 55.0); // boot snapshot stays 100
        let report = verify(&lines);
        assert_eq!(
            report.opens_ok, 1,
            "open must re-derive on the recorded ticket: {report:?}"
        );
        assert_eq!(report.mismatches, 0);
        assert_eq!(report.unverified, 0);
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("notional scaled 55") && n.contains("boot snapshot 100"))
        );
    }

    /// Operator-initiated sell (`sell <mint>`): the runtime records the
    /// tick with `manual_exits` and books the exit Manual without any
    /// market trigger. Replay must reproduce exactly that — and refuse
    /// a pre-flag (or foreign) journal that claims Manual without it,
    /// keeping the flag load-bearing above honesty.
    #[test]
    fn manual_sell_replay_verifies_as_manual_only_with_flag() {
        let manual = |with_flag: bool| {
            let mut settings = boot(1_000, "paper");
            settings.tip_lamports = 1_000;
            let mut book = PaperPortfolio::new(settings.start_balance, settings.fee_bps);
            let mut lines = vec![JournalLine::Boot {
                ts: 1_000,
                settings: settings.clone(),
            }];
            let venue_bps = 125.0f64;
            let sol_feed = 150.0f64;
            book.set_venue_fee_for_mint("M", venue_bps);
            apply_paper_legs(&mut book, sol_feed, &settings);
            let id = book
                .open_position_params(&OpenPositionParams {
                    mint: "M",
                    side: Side::Long,
                    mark: outmark(1.0),
                    notional_usd: settings.notional_usd,
                    tp_percent: 8.0,
                    sl_percent: -20.0,
                    trailing: false,
                    trail_percent: settings.trail_percent,
                    timeout_seconds: 300,
                    moonbag_percent: 0.0,
                    now_secs: 1_001,
                })
                .expect("fixture open");
            let position = book.position(id).unwrap().clone();
            lines.push(JournalLine::Open {
                ts: 1_001,
                mint: "M".into(),
                risk: "heuristic".into(),
                venue_fee_bps: venue_bps,
                sol_feed: Some(sol_feed),
                mark: outmark(1.0),
                ticket_usd: settings.notional_usd,
                position,
            });
            // +5% mark: under the TP/SL thresholds — no market trigger.
            let now = 1_050;
            let mark = outmark(1.05);
            lines.push(JournalLine::Tick {
                ts: now,
                mint: "M".into(),
                mark,
                sol_feed: Some(sol_feed),
                tick_observed_at: now,
                complete: false,
                manual_exits: with_flag,
            });
            let trade = book
                .close_position(id, &mark, ExitReason::Manual, now)
                .expect("operator close");
            lines.push(JournalLine::Exit {
                ts: now,
                mint: "M".into(),
                fraction: 1.0,
                tick_observed_at: now,
                trade,
                mark,
            });
            lines
        };

        let report = verify(&manual(true));
        assert_eq!(report.opens_ok, 1, "{report:?}");
        assert_eq!(report.triggers_ok, 1, "{report:?}");
        assert_eq!(report.closes_ok, 1, "{report:?}");
        assert_eq!(report.mismatches, 0, "{report:?}");
        assert_eq!(report.unverified, 0);

        let report = verify(&manual(false));
        assert!(
            report.trigger_mismatches >= 1,
            "Manual on a market tick without the flag must not be excused: {report:?}"
        );
    }

    /// Pre-upgrade lines without the defaulted fields decode (forwards
    /// compatibility): tick without `manual_exits`, open without
    /// `ticket_usd`, sell attempt without `settle_paired`. Built
    /// through the DTO (serialized, then the defaulted key stripped to
    /// stand in for an older writer) — no hand-typed wire JSON.
    #[test]
    fn pre_upgrade_lines_decode_with_default_fields() {
        let mut tick = serde_json::to_value(&JournalLine::Tick {
            ts: 1,
            mint: "M".into(),
            mark: Mark::price_only(1.0),
            sol_feed: None,
            tick_observed_at: 1,
            complete: false,
            manual_exits: true,
        })
        .unwrap();
        tick.as_object_mut().unwrap().remove("manual_exits");
        let mut open = serde_json::to_value(&JournalLine::Open {
            ts: 1,
            mint: "M".into(),
            risk: "model".into(),
            venue_fee_bps: 125.0,
            sol_feed: None,
            mark: Mark::price_only(1.0),
            ticket_usd: 80.0,
            position: simulator::Position {
                id: 1,
                mint: "M".into(),
                side: Side::Long,
                entry_price: 1.0,
                quantity: 10,
                entry_notional: 10.0,
                entry_fee: 0.1,
                tp_percent: 10.0,
                sl_percent: -20.0,
                trail_percent: 0.0,
                trailing_stop: false,
                best_price: 1.0,
                opened_at: 1,
                timeout_seconds: 60,
                moonbag_percent: 0.0,
            },
        })
        .unwrap();
        let old_ticket = open.as_object_mut().unwrap().remove("ticket_usd");
        assert_eq!(old_ticket, Some(serde_json::Value::from(80.0)));
        let mut sell = serde_json::to_value(&JournalLine::SellAttempt {
            ts: 1,
            mint: "M".into(),
            bundle: "B".into(),
            venue: "curve".into(),
            tokens: 10,
            price_usd: 1.0,
            debit_usd: None,
            settle_paired: true,
        })
        .unwrap();
        sell.as_object_mut().unwrap().remove("settle_paired");

        // Stripped wire still decodes — with the defaults filled in.
        let Decoded::Line(tick_line) = decode_line(&tick.to_string()) else {
            panic!("stripped tick must still decode: {tick}");
        };
        let JournalLine::Tick { manual_exits, .. } = *tick_line else {
            panic!("wrong tag: {tick}");
        };
        assert!(!manual_exits, "default must fill `manual_exits` as false");

        let Decoded::Line(open_line) = decode_line(&open.to_string()) else {
            panic!("stripped open must still decode: {open}");
        };
        let JournalLine::Open { ticket_usd, .. } = *open_line else {
            panic!("wrong tag: {open}");
        };
        assert_eq!(ticket_usd, 0.0, "default must fill `ticket_usd` as 0");

        let Decoded::Line(sell_line) = decode_line(&sell.to_string()) else {
            panic!("stripped sell must still decode: {sell}");
        };
        let JournalLine::SellAttempt { settle_paired, .. } = *sell_line else {
            panic!("wrong tag: {sell}");
        };
        assert!(!settle_paired, "default must fill `settle_paired` as false");
    }

    /// The direct-dispatch path records Attempt + confirmed Outcome:
    /// the attempt owns the debit, the outcome owns the verdict —
    /// each tip and each confirmation counted exactly once.
    #[test]
    fn buy_attempt_then_confirmed_outcome_counts_debit_once() {
        let (mut lines, trade_net) = lifecycle_mode("live", 100.0);
        lines.push(JournalLine::BuyAttempt {
            ts: 1_500,
            mint: "M".into(),
            bundle: "B".into(),
            tokens: 100,
            fill: None,
            debit_usd: Some(0.05),
        });
        lines.push(JournalLine::BuyOutcome {
            ts: 1_501,
            mint: "M".into(),
            bundle: Some("B".into()),
            verdict: "confirmed".into(),
            detail: None,
            debit_usd: None,
        });
        let m = metrics(&lines);
        assert_eq!(m.live_buys_dispatched, 1);
        assert_eq!(m.live_buys_confirmed, 1);
        assert!((m.live_cost_usd - 0.05).abs() < 1e-9, "{m:?}");
        assert!(
            (m.live_net_estimate - (trade_net - 0.05)).abs() < 1e-9,
            "{m:?}"
        );
    }

    /// Live estimate accounting: a PAIRED sell's tip lives inside its
    /// exit Gap line and must not be subtracted again; an UNPAIRED
    /// one is subtracted explicitly. `live_cost_usd` always counts
    /// every recorded debit.
    #[test]
    fn live_estimate_counts_each_tip_exactly_once() {
        let (lines, trade_net) = lifecycle_mode("live", 100.0);
        // Paired settle: gap_usd owns the tip (delta 0 → gap = −tip);
        // the duplicate debit must be ignored.
        let m = metrics(&sell_fixture(lines.clone(), true, -0.09));
        assert!(
            (m.live_net_estimate - (trade_net - 0.09)).abs() < 1e-9,
            "{m:?}"
        );
        assert!((m.live_cost_usd - 0.09).abs() < 1e-9);

        // Unpaired settle: no Gap line, the debit is the explicit
        // subtraction (same result here by construction; the OLD
        // paired bug would print trade_net − 2·tip).
        let m = metrics(&sell_fixture(lines, false, -0.09));
        assert!(
            (m.live_net_estimate - (trade_net - 0.09)).abs() < 1e-9,
            "{m:?}"
        );
        assert!((m.live_cost_usd - 0.09).abs() < 1e-9);
    }

    /// Append a live sell sequence to a paper lifecycle. `gap = true`
    /// mirrors the runtime: SellAttempt(settle_paired) + Gap line
    /// carrying tip inside gap_usd = debit (delta=0 here); `gap =
    /// false` is an orphan settle (no Gap line).
    fn sell_fixture(mut lines: Vec<JournalLine>, paired: bool, gap_usd: f64) -> Vec<JournalLine> {
        let tip = 0.09f64;
        lines.push(JournalLine::SellAttempt {
            ts: 2_000,
            mint: "M".into(),
            bundle: "B".into(),
            venue: "curve".into(),
            tokens: 100,
            price_usd: 1.31,
            debit_usd: Some(tip),
            settle_paired: paired,
        });
        if paired {
            lines.push(JournalLine::Gap {
                ts: 2_000,
                mint: "M".into(),
                leg: "exit".into(),
                paper_price_usd: 1.31,
                realized_price_usd: 1.31,
                qty_tokens: 100.0,
                tip_usd: Some(tip),
                // gap_usd = (settle-paper)*qty − tip: delta 0 → −tip.
                gap_usd,
                venue: "curve".into(),
                liquidity_usd: 50_000.0,
            });
        }
        lines
    }

    #[test]
    fn metrics_buckets_gaps_by_venue() {
        let gap = |leg: &str, venue: &str, gap_usd: f64| JournalLine::Gap {
            ts: 3_000,
            mint: "M".into(),
            leg: leg.into(),
            paper_price_usd: 1.0,
            realized_price_usd: 1.0,
            qty_tokens: 10.0,
            tip_usd: None,
            gap_usd,
            venue: venue.into(),
            liquidity_usd: 50_000.0,
        };
        let lines = vec![
            gap("entry", "curve", -0.5),
            gap("exit", "curve", -1.0),
            gap("exit", "pumpswap", -5.0),
            // Pre-dimension lines read as "unknown", never dropped:
            // rows sum back to the leg totals.
            gap("exit", "", -2.0),
        ];
        let m = metrics(&lines);
        assert_eq!(m.entry_gaps, 1);
        assert_eq!(m.exit_gaps, 3);
        assert!((m.exit_gap_total_usd + 8.0).abs() < 1e-9);
        assert_eq!(m.entry_gap_by_venue.get("curve"), Some(&(1, -0.5)));
        assert_eq!(m.exit_gap_by_venue.get("curve"), Some(&(1, -1.0)));
        assert_eq!(m.exit_gap_by_venue.get("pumpswap"), Some(&(1, -5.0)));
        assert_eq!(m.exit_gap_by_venue.get("unknown"), Some(&(1, -2.0)));
        let rows: usize = m.exit_gap_by_venue.values().map(|(n, _)| n).sum();
        assert_eq!(rows, m.exit_gaps);
    }

    #[test]
    fn replay_fails_on_tampered_reason() {
        let (mut lines, _) = lifecycle();
        if let Some(JournalLine::Exit { trade, .. }) = lines.last_mut() {
            trade.reason = ExitReason::StopLoss;
        }
        let report = verify(&lines);
        assert!(
            report.trigger_mismatches >= 1,
            "tampered reason must fail: {report:?}"
        );
        assert!(
            report.mismatches > 0 && report.details.iter().any(|d| d.contains("recorded reason"))
        );
    }

    #[test]
    fn replay_fails_on_missing_tick_input() {
        let (mut lines, _) = lifecycle();
        // Drop the TP tick: the exit has no causal input left.
        lines.retain(|l| !matches!(l, JournalLine::Tick { ts, .. } if *ts == 1_020));
        let report = verify(&lines);
        assert_eq!(
            report.unverified, 1,
            "missing input must not be assumed: {report:?}"
        );
        assert!(
            report
                .details
                .iter()
                .any(|d| d.contains("tick record missing before this exit"))
        );
    }

    #[test]
    fn replay_marks_dropped_open_record() {
        let (mut lines, _) = lifecycle();
        lines.retain(|l| !matches!(l, JournalLine::Open { .. }));
        let report = verify(&lines);
        assert!(
            report
                .details
                .iter()
                .any(|d| d.contains("open record missing"))
        );
        assert!(report.mismatches > 0);
    }
}

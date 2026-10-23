//! Offline research agent: loss-pattern analysis over the decision journal.
//!
//! READ-ONLY by construction: this module prints an experiment report
//! and owns no write path to config, wallet, or dispatch. A proposal
//! below is paper evidence with halves and thin-data caveats —
//! applying one means: operator edits config, paper backtest, live
//! confirm, explicit approval. The command cannot apply anything, so
//! the approval gate is structural, not a comment.
//!
//! Universe is paper exits joined to entry signals (same join as the
//! replay expectancy tables). Paper patterns are NOT live evidence:
//! every section says so next to the numbers.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::journal;
use crate::replay::{self, MICRO_BIN_MIN, MicroRow, concentration_bin, ratio_bin, regime_label};

/// Loss-cluster row: losing trades sharing one analysis bin.
struct LossCluster {
    label: String,
    n: usize,
    total_loss: f64,
}

/// One candidate exclusion rule evaluated against the universe.
struct Proposal {
    name: String,
    /// Trades the rule would have skipped.
    excluded: usize,
    /// Losses avoided by skipping them (positive USD).
    avoided: f64,
    /// Wins foregone by skipping them (positive USD).
    foregone: f64,
    /// Net delta to the total if the rule had been live
    /// (`avoided − foregone`; positive means the rule helped).
    net_delta: f64,
    /// Same delta on the chronological halves (stability check).
    first_delta: f64,
    second_delta: f64,
}

/// Evaluate one exclusion rule: `exclude` names the trades the rule
/// would have skipped. Losses avoided and wins foregone both count —
/// a rule that dodges losses by also dodging larger wins is a cut,
/// not an improvement. Chronological halves mirror the replay
/// stability check: a rule that worked only in the first half is a
/// fluke, not a filter.
fn evaluate_rule(name: &str, rows: &[MicroRow], exclude: impl Fn(&MicroRow) -> bool) -> Proposal {
    let mut excluded = 0usize;
    // Per half: (losses avoided, wins foregone) in positive USD.
    let mut halves = [(0.0f64, 0.0f64); 2];
    let mut sorted: Vec<&MicroRow> = rows.iter().collect();
    sorted.sort_by_key(|r| r.ts);
    let half = sorted.len() / 2;
    for (i, row) in sorted.iter().enumerate() {
        if !exclude(row) {
            continue;
        }
        excluded += 1;
        // A rule that dodges losses by also dodging larger wins is a
        // cut, not an improvement: both directions are counted and the
        // net tells which.
        let (avoided, foregone) = if row.net_pnl < 0.0 {
            (-row.net_pnl, 0.0)
        } else {
            (0.0, row.net_pnl)
        };
        let slot = &mut halves[usize::from(i >= half)];
        slot.0 += avoided;
        slot.1 += foregone;
    }
    let net_delta = halves.iter().map(|(a, f)| a - f).sum::<f64>();
    let (first_delta, second_delta) = (halves[0].0 - halves[0].1, halves[1].0 - halves[1].1);
    Proposal {
        name: name.to_string(),
        excluded,
        avoided: halves[0].0 + halves[1].0,
        foregone: halves[0].1 + halves[1].1,
        net_delta,
        first_delta,
        second_delta,
    }
}

/// One candidate exclusion rule: display name plus the predicate
/// naming the trades it would have skipped.
type CandidateRule = (String, Box<dyn Fn(&MicroRow) -> bool>);

/// Candidate rules under evaluation. Thresholds are analysis grid
/// points, not recommendations — the report ranks them, the operator
/// judges them (thin rules never clear the floor below).
fn candidate_rules() -> Vec<CandidateRule> {
    let mut rules: Vec<CandidateRule> = Vec::new();
    for cap in [25.0, 30.0, 40.0, 50.0] {
        rules.push((
            format!("top_wallet>{cap:.0}% excluded"),
            Box::new(move |r: &MicroRow| r.micro.top_wallet_share_pct.is_some_and(|c| c > cap)),
        ));
    }
    for floor in [0.5, 0.6, 0.7] {
        rules.push((
            format!("buy_ratio<{floor:.1} excluded"),
            Box::new(move |r: &MicroRow| r.micro.buy_ratio.is_some_and(|b| b < floor)),
        ));
    }
    rules.push((
        "insiders distributing excluded".to_string(),
        Box::new(|r: &MicroRow| r.micro.creator_net_base < 0),
    ));
    rules.push((
        "fading tape excluded".to_string(),
        Box::new(|r: &MicroRow| r.micro.velocity_delta.is_some_and(|v| v < 0.0)),
    ));
    for regime in risk_engine::EXIT_REGIME_NAMES {
        rules.push((
            format!("regime {regime} excluded"),
            Box::new(move |r: &MicroRow| r.regime.as_deref() == Some(regime)),
        ));
    }
    rules
}

/// Losing trades grouped by one binning: `(label, n, total_loss)`,
/// worst total first.
fn loss_clusters(rows: &[MicroRow], key: impl Fn(&MicroRow) -> Option<String>) -> Vec<LossCluster> {
    let mut acc: HashMap<String, (usize, f64)> = HashMap::new();
    for row in rows.iter().filter(|r| r.net_pnl < 0.0) {
        if let Some(label) = key(row) {
            let e = acc.entry(label).or_insert((0, 0.0));
            e.0 += 1;
            e.1 += -row.net_pnl;
        }
    }
    let mut out: Vec<LossCluster> = acc
        .into_iter()
        .map(|(label, (n, total_loss))| LossCluster {
            label,
            n,
            total_loss,
        })
        .collect();
    out.sort_by(|a, b| {
        b.total_loss
            .partial_cmp(&a.total_loss)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

fn print_loss_clusters(title: &str, clusters: &[LossCluster]) {
    println!("  {title} (losers only):");
    if clusters.is_empty() {
        println!("    no losing trades in this cut");
        return;
    }
    for c in clusters {
        let thin = if c.n < MICRO_BIN_MIN {
            " THIN (not evidence)"
        } else {
            ""
        };
        println!(
            "    {}: n={} total loss {:.4} USD avg loss {:.4}{thin}",
            c.label,
            c.n,
            c.total_loss,
            c.total_loss / c.n as f64,
        );
    }
}

/// Compact offline summary for `/research`: universe, net, worst
/// loss cluster (regime cut), best filter proposal. Same inputs and
/// caveats as the CLI report, shrunk to one Discord answer. `None` =
/// no signaled exits yet. Paper-only, never live evidence.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ResearchSummary {
    pub exits: usize,
    pub losses: usize,
    pub wins: usize,
    pub net_pnl: f64,
    pub top_cluster_label: String,
    pub top_cluster_loss: f64,
    pub top_cluster_n: usize,
    pub top_proposal_name: String,
    pub top_proposal_net: f64,
    pub top_proposal_excluded: usize,
    /// True when the top proposal is thin (noise, not evidence).
    pub top_proposal_thin: bool,
}

/// Rank the universe exactly like the CLI report, then keep only the
/// headliners. Pure: the sidecar answers from the same math the
/// operator already sees in `pump-sentry research`.
pub(crate) fn summarize(rows: &[MicroRow]) -> Option<ResearchSummary> {
    if rows.is_empty() {
        return None;
    }
    let losses = rows.iter().filter(|r| r.net_pnl < 0.0).count();
    let net_pnl: f64 = rows.iter().map(|r| r.net_pnl).sum();
    let clusters = loss_clusters(rows, |r| {
        Some(regime_label(r.regime.as_deref()).to_string())
    });
    let mut proposals: Vec<Proposal> = candidate_rules()
        .into_iter()
        .map(|(name, rule)| evaluate_rule(&name, rows, rule))
        .collect();
    proposals.sort_by(|a, b| {
        b.net_delta
            .partial_cmp(&a.net_delta)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let (cluster_label, cluster_loss, cluster_n) = clusters
        .first()
        .map(|c| (c.label.clone(), c.total_loss, c.n))
        .unwrap_or_else(|| ("no losing clusters".to_string(), 0.0, 0));
    let (prop_name, prop_net, prop_excluded, prop_thin) = proposals
        .first()
        .map(|p| {
            (
                p.name.clone(),
                p.net_delta,
                p.excluded,
                p.excluded < MICRO_BIN_MIN,
            )
        })
        .unwrap_or_else(|| ("no proposals".to_string(), 0.0, 0, true));
    Some(ResearchSummary {
        exits: rows.len(),
        losses,
        wins: rows.len().saturating_sub(losses),
        net_pnl,
        top_cluster_label: cluster_label,
        top_cluster_loss: cluster_loss,
        top_cluster_n: cluster_n,
        top_proposal_name: prop_name,
        top_proposal_net: prop_net,
        top_proposal_excluded: prop_excluded,
        top_proposal_thin: prop_thin,
    })
}
fn print_proposals(rows: &[MicroRow]) {
    println!("-- filter proposals (paper-only; halves = stability check) --");
    let mut proposals: Vec<Proposal> = candidate_rules()
        .into_iter()
        .map(|(name, rule)| evaluate_rule(&name, rows, rule))
        .collect();
    proposals.sort_by(|a, b| {
        b.net_delta
            .partial_cmp(&a.net_delta)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for p in &proposals {
        let thin = if p.excluded < MICRO_BIN_MIN {
            " THIN (not evidence)"
        } else {
            ""
        };
        println!(
            "  {}: skipped n={} | avoided loss {:.4} | foregone wins {:.4} | net {:+.4} USD (1st-half {:+.4}, 2nd-half {:+.4}){thin}",
            p.name, p.excluded, p.avoided, p.foregone, p.net_delta, p.first_delta, p.second_delta,
        );
    }
}

/// The decision journal for offline research; human report, then exit
/// code 0 (analysis never fails the exit code — verification is
/// `replay`'s job). `None` file uses the default journal.
pub async fn cmd_research(file: Option<String>) -> ! {
    let path: PathBuf = file
        .map(PathBuf::from)
        .unwrap_or_else(journal::default_journal_path);
    let (lines, blank, bad, read_error) = journal::load_lines(&path);
    println!(
        "journal: {} ({} recorded, {} blank, {} skipped-bad lines)",
        path.display(),
        lines.len(),
        blank,
        bad
    );
    println!("RESEARCH REPORT — paper patterns only, never live evidence.");
    println!("Applying anything below means: operator edits config, paper");
    println!("backtest, live confirm, explicit approval. This command");
    println!("cannot apply anything by construction (no write path).");
    if let Some(error) = read_error {
        // An unreadable journal is not evidence of zero decisions;
        // "nothing recorded" would be a fabricated clean slate.
        println!("journal unreadable: {error} — no patterns report (fail closed)");
        std::process::exit(1);
    }
    if lines.is_empty() {
        println!("nothing to analyze: the bot has not recorded any decisions yet");
        std::process::exit(0);
    }
    let m = replay::metrics(&lines);
    let rows = &m.micro_rows;
    let losses = rows.iter().filter(|r| r.net_pnl < 0.0).count();
    println!(
        "universe: {} paper exits joined to entry signals ({} losses, {} wins/breakeven); {} exits without entry signals excluded",
        rows.len(),
        losses,
        rows.len().saturating_sub(losses),
        m.paper_trades.saturating_sub(rows.len()),
    );
    if rows.is_empty() {
        println!("no signaled exits yet: run with entry signals recording first");
        std::process::exit(0);
    }
    println!("-- loss clusters (where the bleed concentrates) --");
    print_loss_clusters(
        "by regime",
        &loss_clusters(rows, |r| {
            Some(regime_label(r.regime.as_deref()).to_string())
        }),
    );
    // Per-model loss view: the journal spans boots, so this is the
    // cross-run performance read. "Not recorded" covers engine-absent
    // mode and pre-upgrade lines without a name.
    print_loss_clusters(
        "by risk model",
        &loss_clusters(rows, |r| {
            Some(r.model.as_deref().unwrap_or("not recorded").to_string())
        }),
    );
    print_loss_clusters(
        "by buy pressure",
        &loss_clusters(rows, |r| {
            r.micro.buy_ratio.map(|b| ratio_bin(b).to_string())
        }),
    );
    print_loss_clusters(
        "by round concentration",
        &loss_clusters(rows, |r| {
            r.micro
                .top_wallet_share_pct
                .map(|p| concentration_bin(p).to_string())
        }),
    );
    print_proposals(rows);
    println!("HOW TO PROMOTE A PROPOSAL: (1) paper backtest with the knob armed,");
    println!("(2) confirm the halves still agree out-of-sample, (3) live confirm on");
    println!("the live bucket only, (4) explicit operator approval + config edit.");
    println!("A THIN proposal that 'wins' is noise wearing a lab coat.");
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::MicroRow;
    use listener::MicroSignals;

    fn sig(ratio: Option<f64>, conc: Option<f64>, creator: i64) -> MicroSignals {
        MicroSignals {
            buys: 5,
            sells: 1,
            buy_ratio: ratio,
            buy_volume_base: 5_000_000,
            sell_volume_base: 1_000_000,
            unique_traders: 4,
            top_wallet_share_pct: conc,
            creator_net_base: creator,
            velocity_delta: None,
        }
    }

    fn row(ts: u64, net: f64, micro: MicroSignals, regime: Option<&str>) -> MicroRow {
        MicroRow {
            ts,
            net_pnl: net,
            micro,
            regime: regime.map(str::to_string),
            model: Some("test-model".to_string()),
            source: "model".to_string(),
        }
    }

    /// Concentrated losers + dispersed winners: the concentration
    /// proposal must surface positive net delta, the ratio proposal
    /// must not (all ratios healthy here).
    fn fixture_rows() -> Vec<MicroRow> {
        vec![
            row(1, -2.0, sig(Some(0.9), Some(60.0), 0), Some("momentum")),
            row(2, -1.0, sig(Some(0.85), Some(55.0), 0), Some("momentum")),
            row(3, -0.5, sig(Some(0.8), Some(52.0), 0), Some("conservative")),
            row(4, 3.0, sig(Some(0.9), Some(10.0), 0), Some("momentum")),
            row(5, 1.0, sig(Some(0.75), Some(15.0), 0), Some("conservative")),
            row(6, -3.0, sig(Some(0.9), Some(70.0), -100), Some("momentum")),
        ]
    }

    #[test]
    fn summarize_headlines_match_cli_math() {
        let s = summarize(&fixture_rows()).expect("universe nonempty");
        assert_eq!((s.exits, s.losses, s.wins), (6, 4, 2));
        assert!((s.net_pnl - (-2.5)).abs() < 1e-9);
        assert_eq!(s.top_cluster_label, "regime momentum");
        assert!((s.top_cluster_loss - 6.0).abs() < 1e-9);
        assert_eq!(s.top_cluster_n, 3);
        // Concentrated bleed dominates this fixture: some top_wallet
        // rule must win with positive net delta. Six rows make every
        // rule thin by construction — the flag must say so honestly.
        assert!(s.top_proposal_net > 0.0);
        assert!(s.top_proposal_thin);
        assert!(s.top_proposal_excluded < MICRO_BIN_MIN);
        assert_eq!(summarize(&[]), None);
    }

    #[test]
    fn loss_clusters_rank_by_total_loss() {
        let clusters = loss_clusters(&fixture_rows(), |r| {
            Some(regime_label(r.regime.as_deref()).to_string())
        });
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].label, "regime momentum");
        assert!((clusters[0].total_loss - 6.0).abs() < 1e-9);
        assert_eq!(clusters[0].n, 3);
    }

    #[test]
    fn concentration_proposal_beats_ratio_proposal_here() {
        let rows = fixture_rows();
        let conc = evaluate_rule("top_wallet>50% excluded", &rows, |r| {
            r.micro.top_wallet_share_pct.is_some_and(|c| c > 50.0)
        });
        // Skips 4 concentrated trades (-2,-1,-0.5,-3 = -6.5 avoided;
        // no concentrated winners foregone): net +6.5.
        assert_eq!(conc.excluded, 4);
        assert!((conc.avoided - 6.5).abs() < 1e-9);
        assert_eq!(conc.foregone, 0.0);
        assert!((conc.net_delta - 6.5).abs() < 1e-9);
        // Halves: first half (rows 1-3) delta +3.5, second +3.0.
        assert!((conc.first_delta - 3.5).abs() < 1e-9);
        assert!((conc.second_delta - 3.0).abs() < 1e-9);
        // All ratios healthy: the ratio rule skips nothing.
        let ratio = evaluate_rule("buy_ratio<0.5 excluded", &rows, |r| {
            r.micro.buy_ratio.is_some_and(|b| b < 0.5)
        });
        assert_eq!(ratio.excluded, 0);
        assert_eq!(ratio.net_delta, 0.0);
    }

    #[test]
    fn thin_proposals_stay_marked() {
        // Two rows: any exclusion is thin by construction.
        let rows = vec![
            row(1, -1.0, sig(Some(0.4), Some(10.0), 0), None),
            row(2, 2.0, sig(Some(0.9), Some(10.0), 0), None),
        ];
        let p = evaluate_rule("buy_ratio<0.5 excluded", &rows, |r| {
            r.micro.buy_ratio.is_some_and(|b| b < 0.5)
        });
        assert_eq!(p.excluded, 1);
        assert!(p.excluded < MICRO_BIN_MIN);
    }
}

//! Crash log. Write before mutate; replay on boot.
use crate::live::LivePosition;

/// One pending buy.
#[derive(Debug, Clone)]
pub struct WalPending {
    pub position: LivePosition,
    /// Tx signature for re-polls.
    pub signature: String,
    /// Engine bundle UUID (dedup key).
    pub bundle_id: String,
}

/// One human-exit bag.
#[derive(Debug, Clone)]
pub struct WalManual {
    pub position: LivePosition,
    /// Lookup id (bundle, signature, or cause).
    pub note: String,
}

/// One sell dispatch awaiting confirm (resend-guard seed).
#[derive(Debug, Clone)]
pub struct WalSellPending {
    pub mint: String,
    pub bundle_id: String,
    /// Line timestamp (recency window at import).
    pub ts: u64,
}

/// Sell traces newer than this seed the resend guard at boot; older
/// bundles cannot still be inflight (Jito drops them in minutes).
pub const SELL_PENDING_SEED_WINDOW_SECS: u64 = 900;

/// Boot state.
#[derive(Debug, Default, Clone)]
pub struct WalReplay {
    /// Pending buys (recheck on boot).
    pub pending: Vec<WalPending>,
    pub positions: Vec<LivePosition>,
    pub manual: Vec<WalManual>,
    /// Unsettled sell dispatches (guard seeds).
    pub sell_pending: Vec<WalSellPending>,
    /// Corrupt lines skipped.
    pub skipped: usize,
}

impl WalReplay {
    /// `(mint, bundle_id)` pairs fresh enough to seed the resend
    /// guard: the rest are ancient history, never inflight.
    pub fn recent_sell_pending(&self, now_secs: u64) -> Vec<(String, String)> {
        self.sell_pending
            .iter()
            .filter(|s| now_secs.saturating_sub(s.ts) < SELL_PENDING_SEED_WINDOW_SECS)
            .map(|s| (s.mint.clone(), s.bundle_id.clone()))
            .collect()
    }
}

/// One live-WAL line. The file is our own write-ahead log: the writer
/// and the boot replay share this shape, so nothing parses it by hand.
/// Field strictness matches the hand parser it replaces: a line missing
/// a load-bearing field (or carrying the wrong type) is damage — counted
/// in `skipped`, never half-adopted.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum WalLine {
    /// Dispatch recorded BEFORE confirming.
    Pending {
        mint: String,
        curve: String,
        assoc: String,
        amount: u64,
        #[serde(default)]
        creator: Option<String>,
        sig: String,
        bundle: String,
        #[serde(default)]
        ts: Option<u64>,
    },
    /// Dead bundle: retires its pending buy and sell traces.
    Settled {
        bundle: String,
        #[serde(default)]
        ts: Option<u64>,
    },
    /// Sell dispatch awaiting confirm (resend-guard seed).
    SellPending {
        mint: String,
        bundle: String,
        ts: u64,
    },
    /// Confirmed fill (custody).
    Position {
        mint: String,
        curve: String,
        assoc: String,
        amount: u64,
        #[serde(default)]
        creator: Option<String>,
        #[serde(default)]
        bundle: Option<String>,
        #[serde(default)]
        ts: Option<u64>,
    },
    /// Confirmed sell (partial or full).
    Sell {
        mint: String,
        amount: u64,
        #[serde(default)]
        bundle: Option<String>,
        #[serde(default)]
        ts: Option<u64>,
    },
    /// Human-exit bag.
    Manual {
        mint: String,
        curve: String,
        assoc: String,
        amount: u64,
        #[serde(default)]
        creator: Option<String>,
        #[serde(default)]
        note: Option<String>,
        #[serde(default)]
        ts: Option<u64>,
    },
    /// Manual bag taken for exit.
    TakeManual {
        mint: String,
        amount: u64,
        #[serde(default)]
        ts: Option<u64>,
    },
}

/// Position from WAL fields. Corrupt addresses skip the line (counted):
/// the writer only emits canonical base58 or null, so unparseable means
/// a damaged file — never half-adopted custody.
fn position_from(
    mint: &str,
    curve: &str,
    assoc: &str,
    amount: u64,
    creator: Option<&str>,
) -> Option<LivePosition> {
    let parse = |s: &str| executor::parse_pubkey(s).ok();
    Some(LivePosition {
        mint: parse(mint)?,
        curve: parse(curve)?,
        assoc_curve: parse(assoc)?,
        amount_tokens: amount,
        creator: match creator {
            None => None,
            Some(s) => Some(parse(s)?),
        },
    })
}

/// Fold lines in order into boot state.
pub fn replay_lines<'a>(lines: impl Iterator<Item = &'a str>) -> WalReplay {
    use std::collections::{HashMap, HashSet};
    let mut pending: HashMap<String, WalPending> = HashMap::new();
    let mut positions: HashMap<String, LivePosition> = HashMap::new();
    let mut manual: HashMap<String, WalManual> = HashMap::new();
    // Sell dispatches keyed by bundle (multiples per mint survive;
    // the resend guard seeds from the unsettled remainder).
    let mut sell_pending: HashMap<(String, String), u64> = HashMap::new();
    // Fills already subtracted (by bundle id). Old files may carry the
    // same sell twice (two writers pre-fix); without this a partial
    // exit replays twice and the held runner vanishes from tracking.
    let mut seen_sells: HashSet<String> = HashSet::new();
    let mut skipped = 0usize;
    // Same-mint bags merge by summing amounts: custody cares about
    // the total held, and per-bundle granularity would ripple into
    // every mint-keyed tracker downstream. Distinct manual notes
    // collapse to the first (amounts stay exact). A second curve for
    // the same mint should not exist (one curve per mint) — if it
    // ever does, say so loudly instead of silently adopting it.
    let add = |map: &mut HashMap<String, LivePosition>, p: LivePosition| {
        map.entry(p.mint.to_string())
            .and_modify(|e| {
                if e.curve != p.curve || e.assoc_curve != p.assoc_curve {
                    tracing::warn!(mint = %e.mint, kept = %e.curve, dropped = %p.curve,
                        "WAL replay: same mint, different curve — keeping first, amounts still summed");
                }
                e.amount_tokens = e.amount_tokens.saturating_add(p.amount_tokens);
            })
            .or_insert(p);
    };
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parsed: WalLine = match serde_json::from_str(line) {
            Ok(parsed) => parsed,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        match parsed {
            WalLine::Pending {
                mint,
                curve,
                assoc,
                amount,
                creator,
                sig,
                bundle,
                ..
            } => {
                let Some(pos) = position_from(&mint, &curve, &assoc, amount, creator.as_deref())
                else {
                    skipped += 1;
                    continue;
                };
                pending.insert(
                    bundle.clone(),
                    WalPending {
                        position: pos,
                        signature: sig,
                        bundle_id: bundle,
                    },
                );
            }
            WalLine::Settled { bundle, .. } => {
                pending.remove(&bundle);
                sell_pending.retain(|(_, b), _| *b != bundle);
            }
            WalLine::SellPending { mint, bundle, ts } => {
                sell_pending.insert((mint, bundle), ts);
            }
            WalLine::Position {
                mint,
                curve,
                assoc,
                amount,
                creator,
                ..
            } => {
                let Some(pos) = position_from(&mint, &curve, &assoc, amount, creator.as_deref())
                else {
                    skipped += 1;
                    continue;
                };
                add(&mut positions, pos);
            }
            WalLine::Sell {
                mint,
                amount,
                bundle,
                ..
            } => {
                // Exactly-once per fill: a repeated bundle id means the
                // same dispatch was recorded twice, not two fills.
                if let Some(bundle) = bundle
                    && !seen_sells.insert(bundle)
                {
                    continue;
                }
                if let Some(p) = positions.get_mut(&mint) {
                    p.amount_tokens = p.amount_tokens.saturating_sub(amount);
                    if p.amount_tokens == 0 {
                        positions.remove(&mint);
                    }
                }
            }
            WalLine::Manual {
                mint,
                curve,
                assoc,
                amount,
                creator,
                note,
                ..
            } => {
                let Some(pos) = position_from(&mint, &curve, &assoc, amount, creator.as_deref())
                else {
                    skipped += 1;
                    continue;
                };
                // Old lines lack note; keep bag.
                let note = note.unwrap_or_default();
                manual
                    .entry(pos.mint.to_string())
                    .and_modify(|e: &mut WalManual| {
                        e.position.amount_tokens =
                            e.position.amount_tokens.saturating_add(pos.amount_tokens);
                    })
                    .or_insert(WalManual {
                        position: pos,
                        note,
                    });
            }
            WalLine::TakeManual { mint, amount, .. } => {
                // A taken-for-exit bag must not replay as a ghost when
                // the crash lands between take and sell-settle.
                if let Some(entry) = manual.get_mut(&mint) {
                    entry.position.amount_tokens =
                        entry.position.amount_tokens.saturating_sub(amount);
                    if entry.position.amount_tokens == 0 {
                        manual.remove(&mint);
                    }
                }
            }
        }
    }
    WalReplay {
        pending: pending.into_values().collect(),
        positions: positions.into_values().collect(),
        manual: manual.into_values().collect(),
        sell_pending: sell_pending
            .into_iter()
            .map(|((mint, bundle_id), ts)| WalSellPending { mint, bundle_id, ts })
            .collect(),
        skipped,
    }
}

/// Read and replay a file. Missing file is empty.
pub fn replay_file(path: &std::path::Path) -> WalReplay {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let replay = replay_lines(raw.lines());
    if replay.skipped > 0 {
        tracing::warn!(file = %path.display(), skipped = replay.skipped, "live WAL has corrupt lines; skipped (inspect the file)");
    }
    replay
}

/// Compact to surviving state. Call once per boot after import.
pub fn compact_file(path: &std::path::Path, replay: &WalReplay) {
    let mut out = String::new();
    let now = crate::settings::now_secs();
    let creator = |c: Option<&solana_pubkey::Pubkey>| c.map(|c| c.to_string());
    for p in &replay.pending {
        push_line(
            &mut out,
            &WalLine::Pending {
                mint: p.position.mint.to_string(),
                curve: p.position.curve.to_string(),
                assoc: p.position.assoc_curve.to_string(),
                amount: p.position.amount_tokens,
                creator: creator(p.position.creator.as_ref()),
                sig: p.signature.clone(),
                bundle: p.bundle_id.clone(),
                ts: Some(now),
            },
        );
    }
    for s in &replay.sell_pending {
        // Prune stale traces at compaction: beyond the seed window no
        // bundle can still be inflight, so keeping them only grows
        // the file for guard entries that would never seed.
        if now.saturating_sub(s.ts) >= SELL_PENDING_SEED_WINDOW_SECS {
            continue;
        }
        push_line(
            &mut out,
            &WalLine::SellPending {
                mint: s.mint.clone(),
                bundle: s.bundle_id.clone(),
                ts: s.ts,
            },
        );
    }
    for p in &replay.positions {
        push_line(
            &mut out,
            &WalLine::Position {
                mint: p.mint.to_string(),
                curve: p.curve.to_string(),
                assoc: p.assoc_curve.to_string(),
                amount: p.amount_tokens,
                creator: creator(p.creator.as_ref()),
                bundle: None,
                ts: Some(now),
            },
        );
    }
    for m in &replay.manual {
        let p = &m.position;
        push_line(
            &mut out,
            &WalLine::Manual {
                mint: p.mint.to_string(),
                curve: p.curve.to_string(),
                assoc: p.assoc_curve.to_string(),
                amount: p.amount_tokens,
                creator: creator(p.creator.as_ref()),
                note: Some(m.note.clone()),
                ts: Some(now),
            },
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

/// Serialize one line into the compaction buffer. The shape holds only
/// scalars/strings, so a failure would be an engine bug: warned, never
/// panicking (a panic here would strand the boot replay).
fn push_line(out: &mut String, line: &WalLine) {
    match serde_json::to_string(line) {
        Ok(json) => {
            out.push_str(&json);
            out.push('\n');
        }
        Err(e) => {
            tracing::warn!(error = %e, "live WAL compaction: line serialize failed (skipped)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Valid-address test mint (live boundaries parse; the writer only
    /// ever emits canonical base58, so replay tests must too).
    const TMINT: &str = "32ZsJ2yJjwuoBiWE5xnZjG9tKmK3CubbmEzgkQLyQzgD";
    const TCURVE: &str = "36VASLSKLFD2KokjXG7V28veZvXEsyHRKefLonPaAKzv";
    const TASSOC: &str = "3AQTaduKvYWFTu1ExZSQK1hQp5jSZ2yEt4KzsASAufKd";

    fn position_line(mint: &str, amount: u64, bundle: &str) -> String {
        format!(
            "{{\"t\":\"position\",\"mint\":\"{mint}\",\"curve\":\"{TCURVE}\",\"assoc\":\"{TASSOC}\",\"amount\":{amount},\"creator\":null,\"bundle\":\"{bundle}\",\"ts\":1}}"
        )
    }

    fn sell_line(mint: &str, amount: u64, bundle: &str) -> String {
        format!(
            "{{\"t\":\"sell\",\"mint\":\"{mint}\",\"amount\":{amount},\"bundle\":\"{bundle}\",\"ts\":2}}"
        )
    }

    #[test]
    fn duplicate_sell_lines_subtract_once() {
        // The same fill recorded twice (two writers pre-fix, or a
        // retried append): replay must not double-subtract.
        let lines = [
            position_line(TMINT, 1000, "B1"),
            sell_line(TMINT, 1000, "B1"),
            sell_line(TMINT, 1000, "B1"),
        ];
        let replay = replay_lines(lines.iter().map(String::as_str));
        assert!(replay.positions.is_empty());
        assert_eq!(replay.skipped, 0);
    }

    #[test]
    fn duplicate_partial_sell_keeps_runner() {
        // Partial exit recorded twice: the held remainder survives
        // replay with exit automation intact.
        let lines = [
            position_line(TMINT, 1000, "B1"),
            sell_line(TMINT, 600, "B2"),
            sell_line(TMINT, 600, "B2"),
        ];
        let replay = replay_lines(lines.iter().map(String::as_str));
        assert_eq!(replay.positions.len(), 1);
        assert_eq!(replay.positions[0].amount_tokens, 400);
    }

    #[test]
    fn distinct_bundles_each_settle() {
        // Different bundle ids are different fills: both subtract.
        let lines = [
            position_line(TMINT, 1000, "B1"),
            sell_line(TMINT, 600, "B2"),
            sell_line(TMINT, 400, "B3"),
        ];
        let replay = replay_lines(lines.iter().map(String::as_str));
        assert!(replay.positions.is_empty());
    }

    #[test]
    fn sell_without_bundle_still_settles() {
        // Legacy lines may lack the id: fall back to plain subtract
        // rather than dropping the fill.
        let lines = [
            position_line(TMINT, 1000, "B1"),
            format!(r#"{{"t":"sell","mint":"{TMINT}","amount":1000,"ts":2}}"#),
        ];
        let replay = replay_lines(lines.iter().map(String::as_str));
        assert!(replay.positions.is_empty());
    }

    #[test]
    fn sell_pending_seeds_guard_until_settled() {
        // Dispatch-then-crash leaves an unsettled sell trace: replay
        // must surface it for the resend guard, and the terminal
        // `settled` line must retire it.
        let open = format!(r#"{{"t":"sell_pending","mint":"{TMINT}","bundle":"B9","ts":1000}}"#);
        let retired = r#"{"t":"settled","bundle":"B9","ts":1001}"#.to_string();
        let replay = replay_lines([open.as_str()].iter().copied());
        assert_eq!(replay.sell_pending.len(), 1);
        assert_eq!(replay.sell_pending[0].bundle_id, "B9");
        // Fresh trace seeds; ancient trace never (cannot be inflight).
        assert_eq!(replay.recent_sell_pending(1000).len(), 1);
        assert!(
            replay
                .recent_sell_pending(1000 + SELL_PENDING_SEED_WINDOW_SECS)
                .is_empty()
        );
        let replay = replay_lines([open.as_str(), retired.as_str()].iter().copied());
        assert!(replay.sell_pending.is_empty());
        assert!(replay.recent_sell_pending(1001).is_empty());
    }

    #[test]
    fn sell_pending_without_ts_skips_loudly() {
        // New-kind lines always carry ts; a missing one is damage.
        let lines = [format!(
            r#"{{"t":"sell_pending","mint":"{TMINT}","bundle":"B9"}}"#
        )];
        let replay = replay_lines(lines.iter().map(String::as_str));
        assert!(replay.sell_pending.is_empty());
        assert_eq!(replay.skipped, 1);
    }

    #[test]
    fn take_manual_retires_parked_bag() {
        // Park then take for exit: a crash between take and
        // sell-settle must not replay the bag as a ghost.
        let park = format!(
            r#"{{"t":"manual","mint":"{TMINT}","curve":"{TCURVE}","assoc":"{TASSOC}","amount":1000,"creator":null,"note":"sell-retries-exhausted","ts":1}}"#
        );
        let take = format!(r#"{{"t":"take_manual","mint":"{TMINT}","amount":1000,"ts":2}}"#);
        let replay = replay_lines([park.as_str(), take.as_str()].iter().copied());
        assert!(replay.manual.is_empty());
        // Partial take leaves the remainder parked.
        let take_part =
            format!(r#"{{"t":"take_manual","mint":"{TMINT}","amount":600,"ts":2}}"#);
        let replay = replay_lines([park.as_str(), take_part.as_str()].iter().copied());
        assert_eq!(replay.manual.len(), 1);
        assert_eq!(replay.manual[0].position.amount_tokens, 400);
    }
}

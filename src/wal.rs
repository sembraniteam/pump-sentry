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

/// Boot state.
#[derive(Debug, Default, Clone)]
pub struct WalReplay {
    /// Pending buys (recheck on boot).
    pub pending: Vec<WalPending>,
    pub positions: Vec<LivePosition>,
    pub manual: Vec<WalManual>,
    /// Corrupt lines skipped.
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

/// Fold lines in order into boot state.
pub fn replay_lines<'a>(lines: impl Iterator<Item = &'a str>) -> WalReplay {
    use std::collections::{HashMap, HashSet};
    let mut pending: HashMap<String, WalPending> = HashMap::new();
    let mut positions: HashMap<String, LivePosition> = HashMap::new();
    let mut manual: HashMap<String, WalManual> = HashMap::new();
    // Fills already subtracted (by bundle id). Old files may carry the
    // same sell twice (two writers pre-fix); without this a partial
    // exit replays twice and the held runner vanishes from tracking.
    let mut seen_sells: HashSet<String> = HashSet::new();
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
                let (Some(mint), Some(amount)) = (field(line, "mint"), num(line, "amount")) else {
                    skipped += 1;
                    continue;
                };
                // Exactly-once per fill: a repeated bundle id means the
                // same dispatch was recorded twice, not two fills.
                if let Some(bundle) = field(line, "bundle")
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
            "manual" => {
                let Some(pos) = position_from(line) else {
                    skipped += 1;
                    continue;
                };
                // Old lines lack note; keep bag.
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
            crate::settings::now_secs(),
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
            crate::settings::now_secs(),
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
            crate::settings::now_secs(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn position_line(mint: &str, amount: u64, bundle: &str) -> String {
        format!(
            "{{\"t\":\"position\",\"mint\":\"{mint}\",\"curve\":\"C\",\"assoc\":\"A\",\"amount\":{amount},\"creator\":null,\"bundle\":\"{bundle}\",\"ts\":1}}"
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
            position_line("MINT", 1000, "B1"),
            sell_line("MINT", 1000, "B1"),
            sell_line("MINT", 1000, "B1"),
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
            position_line("MINT", 1000, "B1"),
            sell_line("MINT", 600, "B2"),
            sell_line("MINT", 600, "B2"),
        ];
        let replay = replay_lines(lines.iter().map(String::as_str));
        assert_eq!(replay.positions.len(), 1);
        assert_eq!(replay.positions[0].amount_tokens, 400);
    }

    #[test]
    fn distinct_bundles_each_settle() {
        // Different bundle ids are different fills: both subtract.
        let lines = [
            position_line("MINT", 1000, "B1"),
            sell_line("MINT", 600, "B2"),
            sell_line("MINT", 400, "B3"),
        ];
        let replay = replay_lines(lines.iter().map(String::as_str));
        assert!(replay.positions.is_empty());
    }

    #[test]
    fn sell_without_bundle_still_settles() {
        // Legacy lines may lack the id: fall back to plain subtract
        // rather than dropping the fill.
        let lines = [
            position_line("MINT", 1000, "B1"),
            "{\"t\":\"sell\",\"mint\":\"MINT\",\"amount\":1000,\"ts\":2}".to_string(),
        ];
        let replay = replay_lines(lines.iter().map(String::as_str));
        assert!(replay.positions.is_empty());
    }
}

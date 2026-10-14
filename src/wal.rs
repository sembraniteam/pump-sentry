//! Crash-recovery write-ahead log (JSONL, hand-rolled like
//! [`crate::cli::PidInfo`]: base58 values never need escaping).
//!
//! Every custody-changing event appends BEFORE the in-memory map is
//! touched, so a SIGKILL/abort between dispatch and bookkeeping replays
//! to the same state on the next boot instead of stranding funds:
//!
//! - `pending` after a bundle receipt (re-polled on boot),
//! - `settled` once a pending bundle is proven failed (stops re-poll),
//! - `position` once a fill is tracked (restores `live_positions`),
//! - `sell` once a sell confirms (subtracts on replay),
//! - `manual` once a bag parks for human exit (restores `live_manual`).
//!
//! Boot replays in file order (ordered fold) and compacts back to the
//! surviving state. Corrupt lines are skipped with a loud log, never a
//! panic. `None` path (tests, paper) disables all WAL IO.
use crate::live::LivePosition;

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

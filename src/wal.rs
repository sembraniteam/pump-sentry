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
    /// Dispatch blockhash (base58) for expiry proof. `None` on older lines.
    pub blockhash: Option<String>,
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
    /// Tx signature for reconciliation before any resend release.
    /// `None` on older lines: the engine word governs those.
    pub signature: Option<String>,
    /// Dispatch blockhash (base58) for expiry proof. `None` on older
    /// lines (pre-upgrade), same caveat as `signature`.
    pub blockhash: Option<String>,
    /// Line timestamp (recency window at import).
    pub ts: u64,
}

/// Sell traces newer than this seed the resend guard at boot; older
/// bundles cannot still be inflight (Jito drops them in minutes).
pub const SELL_PENDING_SEED_WINDOW_SECS: u64 = 900;

/// One schema stamp seen in the file: writer provenance per boot.
/// Retained through compaction so the file carries its full writer
/// history (one small line per boot — bounded by restart count, and
/// restarts are the very thing being audited).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WalMeta {
    pub schema: u32,
    #[serde(default)]
    pub version: String,
    pub ts: u64,
}

/// Boot state.
#[derive(Debug, Default, Clone)]
pub struct WalReplay {
    /// Pending buys (recheck on boot).
    pub pending: Vec<WalPending>,
    pub positions: Vec<LivePosition>,
    pub manual: Vec<WalManual>,
    /// Unsettled sell dispatches (guard seeds).
    pub sell_pending: Vec<WalSellPending>,
    /// Schema stamps in file order (provenance, never custody).
    pub meta: Vec<WalMeta>,
    /// Corrupt lines skipped.
    pub skipped: usize,
}

impl WalReplay {
    /// Sell traces fresh enough to seed the resend guard: the rest are
    /// ancient history, never inflight.
    pub fn recent_sell_pending(&self, now_secs: u64) -> Vec<WalSellPending> {
        self.sell_pending
            .iter()
            .filter(|s| now_secs.saturating_sub(s.ts) < SELL_PENDING_SEED_WINDOW_SECS)
            .cloned()
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
        blockhash: Option<String>,
        #[serde(default)]
        ts: Option<u64>,
        /// Planned entry cost basis (USD per display token).
        /// `None` on legacy lines: replay carries 0.0 (unknown) and the
        /// settle point counts such exits as unaccounted.
        #[serde(default)]
        entry_usd: Option<f64>,
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
        #[serde(default)]
        sig: Option<String>,
        #[serde(default)]
        blockhash: Option<String>,
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
        /// Planned entry cost basis (USD per display token).
        /// `None` on legacy lines (unknown; counted at settle).
        #[serde(default)]
        entry_usd: Option<f64>,
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
        /// Planned entry cost basis (USD per display token).
        /// `None` on legacy lines (unknown; counted at settle).
        #[serde(default)]
        entry_usd: Option<f64>,
    },
    /// Manual bag taken for exit.
    TakeManual {
        mint: String,
        amount: u64,
        #[serde(default)]
        ts: Option<u64>,
    },
    /// Tracked bag handed from the exit path into the manual notes
    /// (park-for-manual). Retires the tracked claim, so every parked
    /// bag replays with exactly ONE claim : the tracked line
    /// would otherwise stack beside the manual note and read as
    /// double custody. Older binaries skip the unknown tag (counted);
    /// the manual claim keeps the custody whole in both directions.
    TakePosition {
        mint: String,
        amount: u64,
        #[serde(default)]
        ts: Option<u64>,
    },
    /// Schema stamp, written once per boot. Carries NO custody: it is
    /// provenance for versioning/forensics. Old binaries skip unknown
    /// tags (counted in `skipped`, never half-adopted) so the stamp is
    /// forward-compatible in both directions.
    Meta {
        /// WAL schema version ([`WAL_SCHEMA_VERSION`] at write time).
        schema: u32,
        /// Writer version string (crate version at write time).
        #[serde(default)]
        version: String,
        /// Boot stamp (unix secs).
        ts: u64,
    },
}

/// Current WAL schema version. Bump only for a wire-shape change that
/// replay must distinguish (missing load-bearing fields stay behind
/// `#[serde(default)]`, which needs no bump).
///
/// v2 : `entry_usd` on `pending`/`position`/`manual` lines.
/// Old-schema lines replay with `entry_usd_per_token = 0.0` (unknown);
/// the settle point counts their exits as unaccounted instead of
/// silently excluding the realized PnL from the breaker.
pub const WAL_SCHEMA_VERSION: u32 = 2;

/// Position from WAL fields. Corrupt addresses skip the line (counted):
/// the writer only emits canonical base58 or null, so unparseable means
/// a damaged file — never half-adopted custody.
///
/// `Entry_usd` is the per-token cost basis; absent (legacy
/// lines) becomes `0.0` = unknown, and the settle point counts and
/// alerts such exits instead of silently dropping their realized PnL.
fn position_from(
    mint: &str,
    curve: &str,
    assoc: &str,
    amount: u64,
    creator: Option<&str>,
    entry_usd: Option<f64>,
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
        entry_usd_per_token: entry_usd
            .filter(|v| v.is_finite() && *v > 0.0)
            .unwrap_or(0.0),
    })
}

/// WAL-writeable basis: known, finite, positive values serialize; the
/// unknown `0.0` sentinel stays absent (legacy lines parse back as
/// unknown either way).
pub(crate) fn option_entry_usd(per_token: f64) -> Option<f64> {
    (per_token.is_finite() && per_token > 0.0).then_some(per_token)
}

/// Weighted-average cost basis for merged same-mint bags (the amounts
/// are summed; the per-token basis cannot simply sum).
fn merge_entry_basis(a: &LivePosition, b: &LivePosition, merged_amount: u64) -> f64 {
    // One unknown side marks the merged bag unknown (fail-closed):
    // averaging a 0-sentinel with a real basis would dilute the basis
    // and understate a later loss — never silent.
    if a.entry_usd_per_token <= 0.0
        || b.entry_usd_per_token <= 0.0
        || !a.entry_usd_per_token.is_finite()
        || !b.entry_usd_per_token.is_finite()
    {
        return 0.0;
    }
    let total = (a.amount_tokens as f64) * a.entry_usd_per_token
        + (b.amount_tokens as f64) * b.entry_usd_per_token;
    if merged_amount == 0 || !total.is_finite() {
        return 0.0;
    }
    total / merged_amount as f64
}

/// Fold lines in order into boot state.
pub fn replay_lines<'a>(lines: impl Iterator<Item = &'a str>) -> WalReplay {
    use std::collections::{HashMap, HashSet};
    let mut pending: HashMap<String, WalPending> = HashMap::new();
    let mut positions: HashMap<String, LivePosition> = HashMap::new();
    let mut manual: HashMap<String, WalManual> = HashMap::new();
    // Sell dispatches keyed by bundle (multiples per mint survive;
    // the resend guard seeds from the unsettled remainder). Value is
    // `(ts, signature, blockhash)`; older lines lack the last two.
    type SellPendingTrace = (u64, Option<String>, Option<String>);
    let mut sell_pending: HashMap<(String, String), SellPendingTrace> = HashMap::new();
    // Fills already subtracted (by bundle id). Old files may carry the
    // same sell twice (two writers pre-fix); without this a partial
    // exit replays twice and the held runner vanishes from tracking.
    let mut seen_sells: HashSet<String> = HashSet::new();
    let mut meta: Vec<WalMeta> = Vec::new();
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
                let merged = e.amount_tokens.saturating_add(p.amount_tokens);
                e.entry_usd_per_token = merge_entry_basis(e, &p, merged);
                e.amount_tokens = merged;
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
                blockhash,
                entry_usd,
                ..
            } => {
                let Some(pos) =
                    position_from(&mint, &curve, &assoc, amount, creator.as_deref(), entry_usd)
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
                        blockhash,
                    },
                );
            }
            WalLine::Settled { bundle, .. } => {
                pending.remove(&bundle);
                sell_pending.retain(|(_, b), _| *b != bundle);
            }
            WalLine::SellPending {
                mint,
                bundle,
                sig,
                blockhash,
                ts,
            } => {
                sell_pending.insert((mint, bundle), (ts, sig, blockhash));
            }
            WalLine::Position {
                mint,
                curve,
                assoc,
                amount,
                creator,
                bundle,
                entry_usd,
                ..
            } => {
                let Some(pos) =
                    position_from(&mint, &curve, &assoc, amount, creator.as_deref(), entry_usd)
                else {
                    skipped += 1;
                    continue;
                };
                // A confirmed-fill `Position` retires its pending line
                // : the writer only emits `Position` after the
                // chain confirmed, so a crash between the `Position`
                // append and the `Settled` tombstone must not resurrect
                // a phantom custody claim at boot. Legacy/compacted
                // positions without a bundle cannot be attributed (the
                // bundle is preserved going forward by compaction; for
                // old files the pending simply replays as before).
                if let Some(bundle) = bundle {
                    pending.remove(&bundle);
                }
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
                entry_usd,
                ..
            } => {
                let Some(pos) =
                    position_from(&mint, &curve, &assoc, amount, creator.as_deref(), entry_usd)
                else {
                    skipped += 1;
                    continue;
                };
                // Old lines lack note; keep bag.
                let note = note.unwrap_or_default();
                manual
                    .entry(pos.mint.to_string())
                    .and_modify(|e: &mut WalManual| {
                        let merged = e.position.amount_tokens.saturating_add(pos.amount_tokens);
                        e.position.entry_usd_per_token =
                            merge_entry_basis(&e.position, &pos, merged);
                        e.position.amount_tokens = merged;
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
            WalLine::TakePosition { mint, amount, .. } => {
                // The parked manual note is the bag's claim now; the
                // tracked line it replaces must not replay beside it
                // (one claim per held bag).
                if let Some(pos) = positions.get_mut(&mint) {
                    pos.amount_tokens = pos.amount_tokens.saturating_sub(amount);
                    if pos.amount_tokens == 0 {
                        positions.remove(&mint);
                    }
                }
            }
            WalLine::Meta {
                schema,
                version,
                ts,
            } => {
                // Provenance only: no custody to fold — but retained so
                // compaction keeps the file's writer history.
                meta.push(WalMeta {
                    schema,
                    version,
                    ts,
                });
            }
        }
    }
    WalReplay {
        pending: pending.into_values().collect(),
        positions: positions.into_values().collect(),
        manual: manual.into_values().collect(),
        sell_pending: sell_pending
            .into_iter()
            .map(
                |((mint, bundle_id), (ts, signature, blockhash))| WalSellPending {
                    mint,
                    bundle_id,
                    signature,
                    blockhash,
                    ts,
                },
            )
            .collect(),
        meta,
        skipped,
    }
}

/// Read and replay a file. A missing file is an empty WAL; any other
/// read error is returned: an unreadable WAL means custody state is
/// UNKNOWN, so boot must refuse instead of replaying it as empty.
pub fn replay_file(path: &std::path::Path) -> std::io::Result<WalReplay> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WalReplay::default());
        }
        Err(e) => return Err(e),
    };
    let replay = replay_lines(raw.lines());
    if replay.skipped > 0 {
        tracing::warn!(file = %path.display(), skipped = replay.skipped, "live WAL has corrupt lines; skipped (inspect the file)");
    }
    Ok(replay)
}

/// Compact to surviving state. Call once per boot after import.
pub fn compact_file(path: &std::path::Path, replay: &WalReplay) {
    let mut out = String::new();
    let now = crate::settings::now_secs();
    let creator = |c: Option<&solana_pubkey::Pubkey>| c.map(|c| c.to_string());
    // Writer history first: compaction must not erase provenance (the
    // fresh session appends its own stamp after this returns).
    for m in &replay.meta {
        push_line(
            &mut out,
            &WalLine::Meta {
                schema: m.schema,
                version: m.version.clone(),
                ts: m.ts,
            },
        );
    }
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
                blockhash: p.blockhash.clone(),
                ts: Some(now),
                entry_usd: option_entry_usd(p.position.entry_usd_per_token),
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
                sig: s.signature.clone(),
                blockhash: s.blockhash.clone(),
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
                entry_usd: option_entry_usd(p.entry_usd_per_token),
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
                entry_usd: option_entry_usd(p.entry_usd_per_token),
            },
        );
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = crate::cli::atomic_write_private(path, out.as_bytes()) {
        tracing::warn!(file = %path.display(), error = %e, "live WAL compaction failed; original untouched, file keeps growing (non-fatal)");
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

    fn manual_line(amount: u64, note: &str) -> String {
        format!(
            r#"{{"t":"manual","mint":"{TMINT}","curve":"{TCURVE}","assoc":"{TASSOC}","amount":{amount},"creator":null,"note":"{note}","ts":2}}"#
        )
    }

    #[test]
    fn confirmed_fill_position_retires_its_pending_without_a_tombstone() {
        // The writer appends `Pending` then later `Position`
        // then `Settled`. A crash between the `Position` append and the
        // `Settled` tombstone must not resurrect a phantom custody
        // claim at boot: the confirmed fill is the retirement proof.
        let pending = format!(
            r#"{{"t":"pending","mint":"{TMINT}","curve":"{TCURVE}","assoc":"{TASSOC}","amount":1000,"creator":null,"sig":"SIG1","bundle":"B1","ts":1}}"#
        );
        let crash_window = [pending.clone(), position_line(TMINT, 1000, "B1")];
        let replay = replay_lines(crash_window.iter().map(String::as_str));
        assert!(
            replay.pending.is_empty(),
            "a confirmed fill is not a pending"
        );
        assert_eq!(replay.positions.len(), 1);
        assert_eq!(replay.positions[0].amount_tokens, 1000);
        // The normal order is unchanged: `Settled` also removes.
        let settled_window = [
            position_line(TMINT, 1000, "B1"),
            r#"{"t":"settled","bundle":"B1","ts":3}"#.to_string(),
        ];
        let replay = replay_lines(settled_window.iter().map(String::as_str));
        assert!(replay.pending.is_empty());
        assert_eq!(replay.positions.len(), 1);
    }

    #[test]
    fn replay_file_missing_is_empty_unreadable_is_error() {
        // A missing WAL is an empty WAL; an unreadable one is NOT (boot
        // must refuse live: custody would come back unknown).
        let dir = std::env::temp_dir().join(format!("pump-sentry-wal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        let replay = replay_file(&dir.join("no-such.jsonl")).expect("missing file is empty");
        assert!(replay.pending.is_empty() && replay.positions.is_empty());
        // A directory in the WAL's place: EISDIR, not NotFound.
        assert!(replay_file(&dir).is_err(), "unreadable WAL must error");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn torn_final_line_is_skipped_custody_intact() {
        // Crash mid-append: the last line is a JSON prefix without a
        // trailing newline. The line is damage (counted, never
        // half-adopted); the settled custody before it survives.
        let full = position_line(TMINT, 1000, "B1");
        let torn = full[..full.len() / 2].to_string();
        let replay = replay_lines([full, torn].iter().map(String::as_str));
        assert_eq!(replay.skipped, 1, "torn line is counted, not adopted");
        assert_eq!(replay.positions.len(), 1);
        assert_eq!(replay.positions[0].amount_tokens, 1000);
    }

    #[test]
    fn meta_line_is_provenance_not_custody() {
        // The schema stamp decodes and folds as a no-op; it never
        // creates custody and is never counted as damage.
        let meta = WalLine::Meta {
            schema: WAL_SCHEMA_VERSION,
            version: "test".to_string(),
            ts: 7,
        };
        let meta = serde_json::to_string(&meta).expect("meta serializes");
        let replay = replay_lines(
            [
                position_line(TMINT, 1000, "B1"),
                meta,
                position_line(TMINT, 500, "B2"),
            ]
            .iter()
            .map(String::as_str),
        );
        assert_eq!(replay.skipped, 0);
        assert_eq!(replay.positions.len(), 1);
        assert_eq!(replay.positions[0].amount_tokens, 1500);
        // ...but it IS retained for compaction (writer history).
        assert_eq!(
            replay.meta,
            vec![WalMeta {
                schema: WAL_SCHEMA_VERSION,
                version: "test".to_string(),
                ts: 7
            }]
        );
    }

    #[test]
    fn future_meta_schema_still_replays_custody() {
        // A NEWER writer's stamp (schema > ours) is not ours to adopt,
        // but it must not poison the file: the custody lines around it
        // still replay (downgrade reads the ledger, just flags it).
        let future = r#"{"t":"meta","schema":999,"version":"0.0.0","ts":7}"#;
        let replay = replay_lines(
            [position_line(TMINT, 1000, "B1"), future.to_string()]
                .iter()
                .map(String::as_str),
        );
        assert_eq!(replay.skipped, 0);
        assert_eq!(replay.positions.len(), 1);
    }

    #[test]
    fn garbage_lines_are_skipped_without_poisoning() {
        let replay = replay_lines(
            [
                position_line(TMINT, 1000, "B1"),
                "\u{0}\u{1}not json at all".to_string(),
                "{\"t\":\"pending\"}".to_string(), // missing load-bearing fields
                position_line(TMINT, 250, "B2"),
            ]
            .iter()
            .map(String::as_str),
        );
        assert_eq!(replay.skipped, 2);
        assert_eq!(replay.positions.len(), 1);
        assert_eq!(replay.positions[0].amount_tokens, 1250);
    }

    #[test]
    fn torn_write_file_recovery_uses_the_same_rules() {
        // File-level crash recovery: a torn append inside a real file
        // skips the damage and keeps the settled custody (the file
        // layer must agree with the line layer).
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("wal.jsonl");
        let full = position_line(TMINT, 1000, "B1");
        let torn = &full[..full.len() / 2];
        std::fs::write(&path, format!("{full}\n{torn}\n")).expect("write WAL");
        let replay = replay_file(&path).expect("torn WAL is readable");
        assert_eq!(replay.skipped, 1);
        assert_eq!(replay.positions.len(), 1);
        assert_eq!(replay.positions[0].amount_tokens, 1000);
    }

    #[test]
    fn crash_before_the_claim_publishes_replays_the_pending() {
        // The recheck handoff may die right after a Confirmed
        // verdict, before the `Position` claim is written. The pending
        // is the claim of record until then, so replay must still hold
        // it (boot re-imports and re-resolves) — never an invisible
        // confirmed fill.
        let pending = format!(
            r#"{{"t":"pending","mint":"{TMINT}","curve":"{TCURVE}","assoc":"{TASSOC}","amount":1000,"creator":null,"sig":"SIG1","bundle":"B1","ts":1}}"#
        );
        let replay = replay_lines([pending.as_str()].iter().copied());
        assert_eq!(replay.pending.len(), 1, "claim of record survives");
        assert!(replay.positions.is_empty() && replay.manual.is_empty());
    }

    #[test]
    fn parked_from_tracked_bags_claim_exactly_once() {
        // Park side: handing a tracked bag into the manual
        // notes retires the tracked claim, so a parked bag replays with
        // exactly ONE claim (chains of boots never stack, the custody
        // recheck never reads a double count side).
        let park = vec![
            format!(
                r#"{{"t":"position","mint":"{TMINT}","curve":"{TCURVE}","assoc":"{TASSOC}","amount":1000,"creator":null,"bundle":"B1","ts":1}}"#
            ),
            manual_line(1_000, "sell-retries-exhausted"),
            format!(r#"{{"t":"take_position","mint":"{TMINT}","amount":1000,"ts":3}}"#),
        ];
        let replay = replay_lines(park.iter().map(String::as_str));
        assert!(replay.positions.is_empty(), "tracked claim retired at park");
        assert_eq!(replay.manual.len(), 1);
        assert_eq!(replay.manual[0].position.amount_tokens, 1_000);

        // Resurrection (take side): the durable tracked claim
        // happens BEFORE the manual claim retires — a crash at any
        // point replays the manual bag or the tracked claim, never
        // neither. Then the eventual settled sale clears everything.
        let resurrect = [
            park.clone(),
            vec![
                format!(
                    r#"{{"t":"position","mint":"{TMINT}","curve":"{TCURVE}","assoc":"{TASSOC}","amount":1000,"creator":null,"bundle":null,"ts":4}}"#
                ),
                format!(r#"{{"t":"take_manual","mint":"{TMINT}","amount":1000,"ts":5}}"#),
            ],
        ]
        .concat();
        let crash_windows = (
            &resurrect[..1 + park.len()],
            &resurrect[..2 + park.len()],
            &resurrect[..],
        );
        let (before_position, before_take_manual, all) = crash_windows;
        let replay = replay_lines(before_position.iter().map(|s| s.as_str()));
        assert_eq!(replay.manual.len(), 1, "crash pre-claim: manual bag kept");
        let replay = replay_lines(before_take_manual.iter().map(|s| s.as_str()));
        assert_eq!(
            replay.positions.len(),
            1,
            "crash mid-claim: tracked bag kept"
        );
        let replay = replay_lines(all.iter().map(String::as_str));
        assert_eq!(replay.positions.len(), 1, "settled claim: tracked bag");
        assert!(replay.manual.is_empty(), "settled claim: manual retired");
        // And the eventual sell clears the tracked claim exactly once.
        let mut sold = all.to_vec();
        sold.push(sell_line(TMINT, 1_000, "B2"));
        let replay = replay_lines(sold.iter().map(String::as_str));
        assert!(
            replay.positions.is_empty() && replay.manual.is_empty(),
            "one claim, one sale, zero ghosts"
        );
    }

    #[test]
    fn legacy_parked_double_claim_still_nets_clean() {
        // Legacy files carry both the tracked claim and the manual note
        // for the same bag (the old park order). TakeManual clears the
        // manual side; the eventual Sell clears the tracked side — one
        // custody, zero ghosts, across any crash point.
        let lines = [
            format!(
                r#"{{"t":"position","mint":"{TMINT}","curve":"{TCURVE}","assoc":"{TASSOC}","amount":1000,"creator":null,"bundle":"B1","ts":1}}"#
            ),
            manual_line(1_000, "sell-retries-exhausted"),
            format!(r#"{{"t":"take_manual","mint":"{TMINT}","amount":1000,"ts":3}}"#),
            sell_line(TMINT, 1_000, "B2"),
        ];
        let replay = replay_lines(lines.iter().map(String::as_str));
        assert!(replay.positions.is_empty());
        assert!(replay.manual.is_empty());
    }

    #[test]
    fn compaction_preserves_meta_history() {
        // Compaction rewrites custody but keeps every stamp: the file
        // always names all its writers, not just the last session.
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("wal.jsonl");
        let meta_old = serde_json::to_string(&WalLine::Meta {
            schema: WAL_SCHEMA_VERSION,
            version: "0.7.1".to_string(),
            ts: 5,
        })
        .expect("meta serializes");
        let full = position_line(TMINT, 1000, "B1");
        std::fs::write(&path, format!("{meta_old}\n{full}\n")).expect("write WAL");
        let replay = replay_file(&path).expect("readable");
        assert_eq!(replay.meta.len(), 1);
        compact_file(&path, &replay);
        let reread = replay_file(&path).expect("compacted file readable");
        assert_eq!(reread.meta.len(), 1, "stamp survives compaction");
        assert_eq!(reread.meta[0].version, "0.7.1");
        assert_eq!(reread.positions.len(), 1);
        assert_eq!(reread.positions[0].amount_tokens, 1000);
    }

    #[test]
    fn compaction_failure_leaves_original_untouched() {
        // Crash-mid-compaction model: when the staged write fails, the
        // original file must keep its exact bytes. A truncate-in-place
        // rewrite could not promise this (open-then-die = torn WAL).
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("wal.jsonl");
        let original = format!("{}\n", position_line(TMINT, 1000, "B1"));
        std::fs::write(&path, &original).expect("write WAL");
        // Sabotage staging: the temp beside the target is a directory,
        // so the atomic write must fail before the target is touched.
        std::fs::create_dir(path.with_extension("part")).expect("staging blocker");
        let replay = replay_file(&path).expect("readable");
        compact_file(&path, &replay);
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            original,
            "failed compaction never truncates the target"
        );
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
    fn reconciliation_fields_roundtrip_and_stay_backward_readable() {
        // New-shape lines carry blockhash / signature: the expiry and
        // reconcile evidence the guards need after a crash.
        let open_pending = format!(
            "{{\"t\":\"pending\",\"mint\":\"{TMINT}\",\"curve\":\"{TCURVE}\",\"assoc\":\"{TASSOC}\",\"amount\":1000,\"creator\":null,\"sig\":\"S1\",\"bundle\":\"B1\",\"blockhash\":\"BH1\",\"ts\":1}}"
        );
        let open_sell = format!(
            "{{\"t\":\"sell_pending\",\"mint\":\"{TMINT}\",\"bundle\":\"B9\",\"sig\":\"S9\",\"blockhash\":\"BH9\",\"ts\":1000}}"
        );
        let replay = replay_lines([open_pending.as_str(), open_sell.as_str()].iter().copied());
        assert_eq!(replay.pending[0].blockhash.as_deref(), Some("BH1"));
        assert_eq!(replay.sell_pending[0].signature.as_deref(), Some("S9"));
        assert_eq!(replay.sell_pending[0].blockhash.as_deref(), Some("BH9"));
        // Old-shape lines lack both fields: still readable, never skipped.
        let old_pending = format!(
            "{{\"t\":\"pending\",\"mint\":\"{TMINT}\",\"curve\":\"{TCURVE}\",\"assoc\":\"{TASSOC}\",\"amount\":1000,\"creator\":null,\"sig\":\"S1\",\"bundle\":\"B2\",\"ts\":1}}"
        );
        let old_sell =
            format!(r#"{{"t":"sell_pending","mint":"{TMINT}","bundle":"B8","ts":1000}}"#);
        let replay = replay_lines([old_pending.as_str(), old_sell.as_str()].iter().copied());
        assert_eq!(replay.skipped, 0);
        assert_eq!(replay.pending[0].blockhash, None);
        assert_eq!(replay.sell_pending[0].signature, None);
        assert_eq!(replay.sell_pending[0].blockhash, None);
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
        let take_part = format!(r#"{{"t":"take_manual","mint":"{TMINT}","amount":600,"ts":2}}"#);
        let replay = replay_lines([park.as_str(), take_part.as_str()].iter().copied());
        assert_eq!(replay.manual.len(), 1);
        assert_eq!(replay.manual[0].position.amount_tokens, 400);
    }

    #[test]
    fn manual_park_keeps_a_same_mint_position_bag() {
        // Parking is a runtime-map move, not a WAL net: the same mint
        // can hold a parked bag AND a separate re-buy bag, so replay
        // must keep the position copy (netting by key/amount would
        // silently delete tracked custody).
        let rows = [
            position_line(TMINT, 1000, "B1"),
            manual_line(1000, "exhausted"),
            position_line(TMINT, 500, "B3"),
        ];
        let replay = replay_lines(rows.iter().map(String::as_str));
        assert_eq!(replay.positions.len(), 1);
        assert_eq!(replay.positions[0].amount_tokens, 1500);
        assert_eq!(replay.manual.len(), 1);
        assert_eq!(replay.manual[0].position.amount_tokens, 1000);
    }
}

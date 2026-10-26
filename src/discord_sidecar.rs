//! Discord sidecar (docs/discord-sidecar.md §2/§4, rollout steps 2–4).
//!
//! `pump-sentry discord` runs next to the bot: it owns the gateway
//! connection and answers slash commands from on-disk state — never
//! from the trading loop. Tier 1 is read-only (`/status`,
//! `/heartbeat`, `/positions`, `/pnl`, `/watchlist`, `/config`);
//! Tier 2 control (`/sell`, `/pause`, `/resume`) writes only the
//! existing control files (sell-request files, pause marker — the same
//! bytes the CLI writes, via the shared `cli` writers).
//!
//! Guardrails baked in:
//! - Tier 1 reads only: `status.json` (the bot's 10-second trailing
//!   file, `src/status`).
//! - Tier 2 writes only existing control shapes, allowlisted operators
//!   only (`operator_ids`), Confirm/Cancel buttons for `/sell` and
//!   `/resume` (unsafe direction), stale-book and stopped-daemon
//!   refusals for `/sell`, every invocation audited to
//!   `discord-audit.jsonl` + the tracing log.
//! - No custody by construction: no private key, no signing, no
//!   dispatch. One secret: the bot token (env/file,
//!   `DISCORD_BOT_TOKEN[_FILE]`).
//! - Guild-locked: interactions from any other server are denied.
//! - Fail closed: no status file, parse failure, stale data, unknown
//!   mint, or expired/missing confirm answers "no action taken" —
//!   never invented values or half-applied control. Auditability is
//!   per command: `/resume` (unsafe direction) refuses to act without
//!   a durable audit line (no audit record ⇒ no resume); `/sell` and
//!   `/pause` (availability of exits) keep executing on audit failure
//!   — the audit line retries once and the reply names the gap
//!   loudly.
//! - Ack window: handlers do nothing but read one small file (Tier 1)
//!   or write one small file (Tier 2), then respond (well inside
//!   Discord's 3 s).

use alert::discord::truncate_content;
use alert::{Alerter, Card};
use serenity::async_trait;
use serenity::builder::{
    CreateActionRow, CreateButton, CreateCommand, CreateCommandOption, CreateEmbed,
    CreateEmbedFooter, CreateInteractionResponse, CreateInteractionResponseMessage,
};
use serenity::model::application::Interaction;
use serenity::model::application::{
    ButtonStyle, CommandDataOptionValue, CommandInteraction, CommandOptionType,
    ComponentInteraction,
};
use serenity::model::gateway::Ready;
use serenity::model::id::GuildId;
use serenity::prelude::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{error, info, warn};

use crate::settings::{Settings, now_secs};
use crate::status::{self, StatusReport};

/// Slash-command names answered by this step (Tier 1 subset).
pub const STATUS_COMMAND: &str = "status";
pub const HEARTBEAT_COMMAND: &str = "heartbeat";
pub const POSITIONS_COMMAND: &str = "positions";
pub const PNL_COMMAND: &str = "pnl";
pub const WATCHLIST_COMMAND: &str = "watchlist";
pub const CONFIG_COMMAND: &str = "config";
/// Tier 3 — read-only analysis (guild-locked, ephemeral; never
/// control, never audited as control).
pub const RESEARCH_COMMAND: &str = "research";
pub const RISK_COMMAND: &str = "risk";
pub const TOKEN_COMMAND: &str = "token";
/// Tier 2 control commands (allowlisted operators only, §6).
pub const SELL_COMMAND: &str = "sell";
pub const PAUSE_COMMAND: &str = "pause";
pub const RESUME_COMMAND: &str = "resume";

/// A status older than this (6 missed 10 s writes) reads as "bot may
/// be down" instead of as fresh state.
pub const STALE_WARN_SECS: u64 = 60;

/// Confirm-button lifetime: no click within this window is a silent
/// no-op (logged, audited as expired). Memory-only: a sidecar restart
/// invalidates every outstanding button (fail closed — re-issue).
pub const CONFIRM_TTL_SECS: u64 = 300;

/// Outstanding confirm buttons per sidecar process. A new confirm at
/// the cap is refused (the operator cancels or waits out outstanding
/// ones first): no silent eviction can strand a pending action that a
/// click may still resolve.
pub const PENDING_CONFIRM_CAP: usize = 64;

/// Interval between expired-confirm sweeps ("no click" entries are
/// dropped when the TTL passes, not only when some button is clicked).
pub const CONFIRM_SWEEP_INTERVAL_SECS: u64 = 60;

/// Tier 2 audit log beside the state files (doc §6/§8: a separate
/// sidecar log, not `journal.jsonl` — the journal schema is the bot's
/// trading record and Tier 2 must not reshape it). One JSON object per
/// line: who/when/command/args/outcome. Never only in Discord.
pub const AUDIT_FILE: &str = "discord-audit.jsonl";

/// Sidecar boot config (`discord.toml` next to `config.toml`,
/// `DISCORD_CONFIG_FILE` override). Kept out of the bot's
/// `config.toml` (doc §8): the sidecar is a different process with a
/// different blast radius.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SidecarConfig {
    /// Discord guild whose members may ask the sidecar things.
    /// Guild-locked (doc §6): everything else is denied.
    pub guild_id: u64,
    /// Discord user IDs allowed Tier 2 control (`/sell`, `/pause`,
    /// `/resume`). Empty (default) = nobody: Tier 2 denies everyone
    /// until the operator lists IDs (fail closed, never fail open).
    #[serde(default)]
    pub operator_ids: Vec<u64>,
}

/// Tier 2 gate: guild first (existing rule), then allowlist. Tier 1
/// stays guild-only; only control checks this list.
fn is_operator(operator_ids: &[u64], user_id: u64) -> bool {
    operator_ids.contains(&user_id)
}

/// Sidecar config path: own env override or beside `config.toml`.
pub fn resolve_config_path() -> std::path::PathBuf {
    if let Ok(v) = std::env::var("DISCORD_CONFIG_FILE") {
        let v = v.trim();
        if !v.is_empty() {
            if let Some(rest) = v.strip_prefix("~/").or_else(|| (v == "~").then_some(""))
                && let Some(home) = std::env::var_os("HOME")
            {
                return std::path::PathBuf::from(home).join(rest);
            }
            return std::path::PathBuf::from(v);
        }
    }
    crate::config::resolve_config_path().with_file_name("discord.toml")
}

/// Parse the sidecar config. Unknown keys are a hard error (a typo'd
/// `guild_id` key must never silently unregister every command).
pub fn parse_config(raw: &str) -> Result<SidecarConfig, String> {
    toml::from_str::<SidecarConfig>(raw).map_err(|e| e.to_string())
}

/// Read the sidecar config from disk, telling the operator exactly
/// what to write when it is missing (fail-stop: no guild, no commands).
pub fn load_config() -> Result<(std::path::PathBuf, SidecarConfig), String> {
    let path = resolve_config_path();
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "cannot read {}: {e} (it must start with `guild_id = <numeric id>`)",
            path.display()
        )
    })?;
    let cfg = parse_config(&raw)?;
    if cfg.guild_id == 0 {
        return Err(format!(
            "{}: `guild_id = 0` is not a Discord id — paste your server's id (enable Developer Mode in Discord, right-click the server, Copy Server ID)",
            path.display()
        ));
    }
    Ok((path, cfg))
}

/// Compact `/status` answer (pure). Operator-only reply: the halted
/// breaker line can carry the day figure, so `/status` is rendered
/// ephemerally (`render_status`); nothing ledger-money is public.
fn status_compact_text(report: &StatusReport, now: u64) -> String {
    let r = &report.report;
    let age = now.saturating_sub(report.ts);
    let uptime_h = r.uptime_secs / 3_600;
    let uptime_m = (r.uptime_secs % 3_600) / 60;
    let oldest_h = r.oldest_open_age_secs / 3_600;
    let oldest_m = (r.oldest_open_age_secs % 3_600) / 60;
    let brk = if report.day_tally_damaged {
        "HALTED (tally damaged)".to_string()
    } else {
        Alerter::breaker_line(r.breaker_halted, r.day_net)
    };
    let data = if r.data_paused {
        "`DEGRADED — entries paused`"
    } else {
        "`ok`"
    };
    let rpc = format!(
        "{h}/{e} ok · failovers {f}{inc}",
        h = r.rpc_healthy,
        e = r.rpc_endpoints,
        f = r.rpc_failover_reads,
        inc = if r.rpc_inconsistent {
            " · INCONSISTENT"
        } else {
            ""
        },
    );
    format!(
        "**Status** — `{m}·{s}` · up {h}h {mm}m\n\
             Breaker: {brk} · Data: {data} · RPC: `{rpc}`\n\
             Open {o} · bags {b} · diverged {d} · pending buys {p} · watchlist {w}\n\
             Oldest open: {oh}h {om}m\n\
             status age: {age}s",
        m = r.mode,
        s = r.strategy,
        h = uptime_h,
        mm = uptime_m,
        brk = brk,
        data = data,
        rpc = rpc,
        o = r.open_positions,
        b = r.manual_bags,
        d = r.diverged,
        p = r.pending_buys,
        w = r.watchlist_pending,
        oh = oldest_h,
        om = oldest_m,
        age = age,
    )
}

/// Honest no-data answer (fail closed, doc §6): the reason drives it.
fn no_status_text(reason: &str) -> String {
    format!(
        "⚠ No readable on-disk status ({reason}) — the bot is not running, is older than sidecar support, or its log dir differs.\n\
         Nothing to report, no action taken."
    )
}

/// Shared `/sell` gate core (pure, test-pinned): the rejection text for
/// a stale book or a stopped daemon, `None` when the gate passes. The
/// stale check comes first so callers can classify outcomes by whether
/// the text contains "Status is": a stale book means "control refuses
/// a stale state", a stopped daemon means "noting would consume the
/// request". Both directions fail closed (doc §6).
fn sell_gate_text(ts: u64, now: u64, daemon_alive: bool) -> Option<String> {
    let age = now.saturating_sub(ts);
    if age > STALE_WARN_SECS {
        return Some(format!(
            "⚠ Status is {age}s old (limit {STALE_WARN_SECS}s) — the bot may be down. Control refuses a stale book: no action taken."
        ));
    }
    if !daemon_alive {
        return Some(
            "No daemon running — a sell request would sit unread. Start the bot first. No action taken."
                .to_string(),
        );
    }
    None
}

/// Full `/heartbeat` answer: the exact production card (text fallback
/// + embed) with the trailing-file freshness note.
///
/// A damaged tally cannot print its day figures as numbers. The card's
/// day-net line is the collector's finite placeholder, so a loud
/// caveat rides alongside it.
fn heartbeat_card(report: &StatusReport, now: u64) -> Card {
    let age = now.saturating_sub(report.ts);
    let mut card = Alerter::format_heartbeat(&report.report);
    card.content = truncate_content(&format!("{}\n(status age: {age}s)", card.content));
    if report.day_tally_damaged {
        card.content = truncate_content(&format!(
            "{}\n⚠ Day-loss tally damaged — day figures unknown, breaker fail-closed HALTED",
            card.content
        ));
    }
    card
}

/// `/heartbeat` answer description: only the lines that ADD to the
/// production card's structured embed — reading freshness plus the
/// damaged-tally caveat. The full text view would mirror every embed
/// field inside the same message.
fn heartbeat_notes(report: &StatusReport, now: u64) -> String {
    let age = now.saturating_sub(report.ts);
    let mut notes = format!("status age: {age}s");
    if report.day_tally_damaged {
        notes.push_str(
            "\n⚠ Day-loss tally damaged — day figures unknown, breaker fail-closed HALTED",
        );
    }
    notes
}

/// Turn the alert crate's embed view into a serenity embed (1:1 by
/// intent: same title/palette/url/footer/fields). An empty title is
/// skipped entirely (nothing sends `title: ""`; unused in a card).
fn embed_from(embed: &alert::discord::Embed) -> CreateEmbed {
    let mut out = CreateEmbed::new().colour(embed.color);
    if !embed.title.is_empty() {
        out = out.title(&embed.title);
    }
    if let Some(url) = &embed.url {
        out = out.url(url);
    }
    if let Some(text) = embed.footer.as_ref().map(|f| &f.text) {
        out = out.footer(CreateEmbedFooter::new(text));
    }
    out.fields(
        embed
            .fields
            .iter()
            .map(|f| (f.name.as_str(), f.value.as_str(), f.inline)),
    )
}

/// Interaction-reply embed (embed-only wire, module-wide contract):
/// the reply text rides the description at Discord's description cap,
/// grey palette. Every answer — panel, refusal, prompt, outcome edit —
/// renders exactly one embed and never a bare `content` string.
fn reply_embed(body: &str) -> CreateEmbed {
    CreateEmbed::new()
        .colour(alert::discord::COLOR_GREY)
        .description(alert::discord::truncate_description(body))
}

/// Money for ledger text: `$10.75`.
fn usd(v: f64) -> String {
    if v < 0.0 {
        format!("-${:.2}", -v)
    } else {
        format!("${v:.2}")
    }
}

/// Signed money for pnl deltas: direction must never be inferred
/// and the currency head belongs beside the magnitude
/// (`+$1.68` / `-$1.25`).
fn signed_usd(v: f64) -> String {
    if v > 0.0 {
        format!("+${v:.2}")
    } else if v < 0.0 {
        format!("-${:.2}", -v)
    } else {
        String::from("$0.00")
    }
}

/// Signed percent for unrealized progress.
fn signed_pct(v: f64) -> String {
    format!("{v:+.1}%")
}

/// Whole-token count in operator units (`2.40M`, `1.2K`, `42`).
fn tok_count(q: u64) -> String {
    let q = q as f64;
    if q >= 1_000_000.0 {
        format!("{:.2}M", q / 1_000_000.0)
    } else if q >= 1_000.0 {
        format!("{:.1}K", q / 1_000.0)
    } else {
        format!("{:.0}", q)
    }
}

/// Duration in operator units (`45s`, `13m`, `2h 5m`, `1d 3h`).
fn dur(secs: u64) -> String {
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3_600;
    let m = (secs % 3_600) / 60;
    match (d, h, m) {
        (0, 0, 0) => format!("{secs}s"),
        (0, 0, _) => format!("{m}m"),
        (0, _, 0) => format!("{h}h"),
        (0, _, _) => format!("{h}h {m}m"),
        (_, 0, _) => format!("{d}d"),
        (_, _, _) => format!("{d}d {h}h"),
    }
}

/// Short mint for inline lines: first four, last four. The full
/// address rides on any explorer click; chat lines stay readable.
fn short_mint(mint: &str) -> String {
    let chars = mint.chars().count();
    if chars <= 12 {
        return mint.to_string();
    }
    let head: String = mint.chars().take(4).collect();
    let tail: String = mint.chars().skip(chars - 4).collect();
    format!("{head}…{tail}")
}

/// Comma-separated short mints for inline lists.
fn mints_inline(mints: &[String]) -> String {
    mints
        .iter()
        .map(|m| short_mint(m))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Prepends the loud staleness banner when the trailing file is old
/// and appends the honest age line (shared by the money panels: even
/// an ephemeral answer must never read a dead bot as current truth).
fn with_freshness(text: String, report: &StatusReport, now: u64) -> String {
    let age = now.saturating_sub(report.ts);
    let mut out = if age > STALE_WARN_SECS {
        format!("⚠ Status is {age}s old (limit {STALE_WARN_SECS}s) — bot may be down.\n\n")
    } else {
        String::new()
    };
    out.push_str(&truncate_content(&text));
    out.push_str(&format!("\nstatus age: {age}s"));
    out
}

/// `/positions` answer (pure): entry sizes and PnL are ledger data —
/// the reply is ephemeral per the doc contract. Unpriced positions
/// read as "held at basis", never as invented marks; live custody the
/// paper book does not mirror is watch-only disclosure.
fn positions_text(report: &StatusReport, now: u64) -> String {
    let r = &report.report;
    let pnl = &report.pnl;
    let open_count = r.open_positions;
    if open_count == 0 {
        let mut out = String::from("**Positions** — none open.");
        if !report.manual_mints.is_empty() {
            out.push_str(&format!(
                "\nUnpaired live bags (on-chain, outside the paper book): {}",
                mints_inline(&report.manual_mints)
            ));
        }
        if !report.pending_buy_mints.is_empty() {
            out.push_str(&format!(
                "\nPending buys (unconfirmed, watch-only): {}",
                mints_inline(&report.pending_buy_mints)
            ));
        }
        return out;
    }
    let exit_discount = pnl.mark_value - pnl.realizable_value;
    let mut out = format!(
        "**Positions** — {} open · mark {} · exit-discount {} (impact+fees if closed now)",
        open_count,
        usd(pnl.mark_value),
        usd(exit_discount)
    );
    for p in &report.positions {
        let held = now.saturating_sub(p.opened_at);
        let time_part = if p.timeout_seconds == 0 {
            "no timeout".to_string()
        } else if held >= p.timeout_seconds {
            format!("⚠ STUCK +{} past timeout", dur(held - p.timeout_seconds))
        } else {
            format!("timeout in {}", dur(p.timeout_seconds - held))
        };
        let mark_part = match (p.mark_price, p.unrealized_usd) {
            (Some(mark), Some(unreal)) => {
                let pct = if p.entry_notional > 0.0 {
                    signed_pct(100.0 * unreal / p.entry_notional)
                } else {
                    "—".to_string()
                };
                format!("mark {mark} ({pct} · {})", signed_usd(unreal))
            }
            _ => "no fresh mark — held at basis".to_string(),
        };
        out.push_str(&format!(
            "\n#{id} {mint} — {qty} tok @ {entry} = {basis} · {mark_part} · held {held} · {time_part}",
            id = p.id,
            mint = short_mint(&p.mint),
            qty = tok_count(p.quantity),
            entry = p.entry_price,
            basis = usd(p.entry_notional),
            held = dur(held),
        ));
    }
    if open_count > report.positions.len() {
        out.push_str(&format!(
            "\n… {} more not shown (file cap {})",
            open_count - report.positions.len(),
            status::POSITIONS_MAX_PUBLISHED
        ));
    }
    if !report.manual_mints.is_empty() {
        out.push_str(&format!(
            "\nUnpaired live bags (watch-only): {}",
            mints_inline(&report.manual_mints)
        ));
    }
    if !report.pending_buy_mints.is_empty() {
        out.push_str(&format!(
            "\nPending buys (unconfirmed, watch-only): {}",
            mints_inline(&report.pending_buy_mints)
        ));
    }
    out
}

/// `/pnl` answer (pure): the whole ledger panel discloses (that is
/// why the reply is ephemeral). The day figure rides the flattened
/// heartbeat (the breaker's day net); book figures come from one
/// snapshot, cash included.
fn pnl_text(report: &StatusReport) -> String {
    let pnl = &report.pnl;
    let r = &report.report;
    let win = match pnl.win_rate {
        Some(w) => format!("{:.0}%", 100.0 * w),
        None => "—".to_string(),
    };
    let exit_discount = pnl.mark_value - pnl.realizable_value;
    let cash_line = if pnl.overdraft_usd > 0.0 {
        format!(
            "Cash {} (⚠ OVERDRAWN by {}) · equity {} (carries the shortfall)",
            usd(r.cash_usd),
            usd(pnl.overdraft_usd),
            usd(r.equity_usd)
        )
    } else {
        format!("Cash {} · equity {}", usd(r.cash_usd), usd(r.equity_usd))
    };
    // Breaker marker, figure-free: the day net already rides the day
    // line beside it, so a halting breaker must not print it again.
    let brk = if report.day_tally_damaged {
        "HALTED (tally damaged)".to_string()
    } else if r.breaker_halted {
        "`HALTED`".to_string()
    } else {
        "`armed`".to_string()
    };
    format!(
        "**PnL** — day {} · breaker {brk}\n\
         Book lifetime: realized {} over {} closed · win rate {win}\n\
         Open book: unrealized {} (realizable basis) · mark {} → would net {} closing full size (exit-discount {})\n\
         {cash_line}",
        if report.day_tally_damaged {
            "UNKNOWN (tally damaged)".to_string()
        } else {
            signed_usd(r.day_net)
        },
        signed_usd(pnl.realized_pnl),
        pnl.closed_count,
        signed_usd(pnl.unrealized_pnl),
        usd(pnl.mark_value),
        usd(pnl.realizable_value),
        usd(exit_discount),
    )
}

/// `/watchlist` answer (pure): mints are chain data and buyers are
/// anonymous counts, so this one stays public. Thresholds ride with
/// the entries so progress reads against a target.
fn watchlist_text(report: &StatusReport, now: u64) -> String {
    let wl = &report.watchlist;
    if wl.total == 0 {
        return String::from(
            "Watchlist empty — dev-launch signals land here and wait for buyer quorum.",
        );
    }
    let head = if wl.total > wl.entries.len() {
        format!(
            "**Watchlist** — {} pending (window {}s, showing {} of {})",
            wl.total,
            wl.window_secs,
            wl.entries.len(),
            wl.total
        )
    } else {
        format!(
            "**Watchlist** — {} pending (window {}s)",
            wl.total, wl.window_secs
        )
    };
    let mut out = head;
    for e in &wl.entries {
        let phase = if e.deadline_secs <= now {
            "⚠ deadline passed (aborts at next poll)".to_string()
        } else {
            format!("expires in {}", dur(e.deadline_secs - now))
        };
        out.push_str(&format!(
            "\n{mint} — {buyers}/{min} buyers · {phase}",
            mint = short_mint(&e.mint),
            buyers = e.buyers,
            min = wl.min_buyers,
            phase = phase,
        ));
    }
    out
}

/// Journal lines read per `/research` answer: bounded tail, never a
/// full replay (memory stays flat no matter how large the file grows).
/// Lines are the cap on what is KEPT; bytes bound what is SCANNED (a
/// multi-GB journal must not stretch the 3-second ack).
/// `load_recent_lines` counts everything it scans, so coverage stays
/// honest (`last N of M`), and its `truncated` flag keeps the byte
/// window loud when it skips history.
const RESEARCH_JOURNAL_TAIL: usize = 5000;
const RESEARCH_JOURNAL_MAX_BYTES: u64 = 8 << 20;
/// Journal lines scanned per `/token` answer (filtered to one mint,
/// newest few shown). Same two-cap discipline as `/research`.
const TOKEN_JOURNAL_TAIL: usize = 2000;
const TOKEN_JOURNAL_MAX_BYTES: u64 = 1 << 20;
/// Trail lines shown per mint (newest last).
const TOKEN_TRAIL_SHOWN: usize = 6;

/// `/research` answer (pure): paper patterns only, never live
/// evidence. Same math as `pump-sentry research`, shrunk to one
/// ephemeral answer. Coverage names the scanned span exactly (whole
/// file, line tail, or bounded byte tail with history skipped) and
/// discloses unreadable lines within it.
fn research_text(
    s: &crate::research::ResearchSummary,
    tail: &crate::journal::JournalTail,
) -> String {
    let damage = if tail.bad > 0 {
        format!(", {} unreadable skipped", tail.bad)
    } else {
        String::new()
    };
    let coverage = if tail.truncated {
        format!(
            "journal tail read, last {scanned} lines (older history not scanned{damage})",
            scanned = tail.scanned,
            damage = damage,
        )
    } else if tail.scanned > tail.lines.len() {
        format!(
            "journal live read, last {} of {} lines{damage}",
            tail.lines.len(),
            tail.scanned
        )
    } else {
        format!("journal live read, {} lines{damage}", tail.scanned)
    };
    let thin = if s.top_proposal_thin {
        " — THIN (noise, not evidence)"
    } else {
        ""
    };
    format!(
        "**Research** — paper patterns only, never live evidence ({coverage})\n\
         Universe: {exits} signaled exits · {losses} losses · {wins} wins · net {net}\n\
         Worst cluster: {clabel} — n={cn} total loss {closs}\n\
         Top proposal: {pname} — net {pnet} over {pex} skipped{thin}\n\
         Promote only via: paper backtest → halves agree → live confirm → explicit approval.",
        exits = s.exits,
        losses = s.losses,
        wins = s.wins,
        net = signed_usd(s.net_pnl),
        clabel = s.top_cluster_label,
        cn = s.top_cluster_n,
        closs = usd(s.top_cluster_loss),
        pname = s.top_proposal_name,
        pnet = signed_usd(s.top_proposal_net),
        pex = s.top_proposal_excluded,
    )
}

/// `/risk` answer (pure): limit utilization quoting the gate's actual
/// enforcement (`src/gate.rs`) from the exact inputs the collector
/// publishes — not a parallel formula:
/// - Heat: `open_positions + heat_live_extra >= max` refuses; the
///   sidecar prints the same sum and the same admission rule.
/// - Exposure: `deployed + notional > cap` refuses, with
///   `deployed = paper_locked + live_bags × notional`.
/// - Day loss: `day_net <= -cap` trips; cap `0` is OFF and never
///   renders `armed`.
/// - `day_tally_damaged`: the tally is unknown (poison/latch) —
///   figures print UNKNOWN and the halt attributes to persistence,
///   never to the loss cap.
fn risk_text(s: &Settings, report: &StatusReport) -> String {
    let r = &report.report;
    let cap = s.max_daily_loss_usd;
    let day_line = if report.day_tally_damaged {
        "day UNKNOWN (tally damaged — restart-persistent halt, delete the day-state file to resume or wait for UTC rollover)".to_string()
    } else if cap.is_finite() && cap > 0.0 {
        let used_pct = if r.day_net < 0.0 {
            100.0 * (-r.day_net) / cap
        } else {
            0.0
        };
        format!(
            "day {} vs cap {} ({:.0}% used)",
            signed_usd(r.day_net),
            usd(cap),
            used_pct
        )
    } else {
        format!(
            "day {} vs cap off — no day-loss breaker",
            signed_usd(r.day_net)
        )
    };
    // Same sum the gate compares: paper opens plus live-held extras
    // refuse at `>= max`, so the gate admits while used < max.
    let heat_used = r.open_positions + report.heat_live_extra;
    let heat_line = format!(
        "Opens {o} + {live} live-held = {used}/{max} (gate admits while used < {max})",
        o = r.open_positions,
        live = report.heat_live_extra,
        used = heat_used,
        max = s.max_open_positions,
    );
    // Same dollars the gate compares: deployed plus the prospective
    // ticket refuses past the cap.
    let deployed = report.exposure_paper_locked + report.exposure_live_bags as f64 * s.notional_usd;
    let prospective = deployed + s.notional_usd;
    let exposure_line = if s.max_exposure_usd.is_finite() && s.max_exposure_usd > 0.0 {
        format!(
            "exposure {} + next {} ticket = {} vs cap {} ({:.0}% committed)",
            usd(deployed),
            usd(s.notional_usd),
            usd(prospective),
            usd(s.max_exposure_usd),
            100.0 * prospective / s.max_exposure_usd,
        )
    } else {
        format!(
            "exposure {} + next {} ticket vs cap off",
            usd(deployed),
            usd(s.notional_usd),
        )
    };
    // Breaker attribution: damage halts regardless of the cap (never
    // blame the loss cap for it). Unhalted, `armed` only renders for a
    // real cap; an undamaged halt prints figure-free — day and cap
    // already ride the utilization line above, never printed twice.
    let breaker_line = if report.day_tally_damaged {
        "HALTED — tally damaged (fail-closed), not the loss cap".to_string()
    } else if r.breaker_halted {
        "`HALTED`".to_string()
    } else if cap.is_finite() && cap > 0.0 {
        "`armed`".to_string()
    } else {
        String::from("cap off (day-loss breaker disabled)")
    };
    format!(
        "**Risk** — limit utilization (caps from config.toml as read just now)\n\
         Day loss: {day}\n\
         {heat} · {exposure}\n\
         Breaker: {brk} · Entries: {entries} · Data: {data}",
        day = day_line,
        heat = heat_line,
        exposure = exposure_line,
        brk = breaker_line,
        entries = if crate::cli::marker_present(&crate::cli::entries_paused_path()) {
            "`PAUSED` via kill switch"
        } else {
            "`open`"
        },
        data = if r.data_paused {
            "`DEGRADED — entries paused`"
        } else {
            "`ok`"
        },
    )
}

/// Mint behind one journal line (`None` for bookkeeping lines that
/// carry no mint). Tick lines are noise for a trail and skipped.
fn journal_mint(line: &crate::journal::JournalLine) -> Option<&str> {
    use crate::journal::JournalLine as J;
    match line {
        J::Open { mint, .. }
        | J::Exit { mint, .. }
        | J::BuyAttempt { mint, .. }
        | J::BuySkip { mint, .. }
        | J::BuyOutcome { mint, .. }
        | J::SellAttempt { mint, .. }
        | J::SellGiveUp { mint, .. }
        | J::Gap { mint, .. } => Some(mint),
        J::Tick { .. } | J::Boot { .. } | J::Equity { .. } => None,
    }
}

/// One trail line, newest last. Only decision-carrying variants
/// render; ticks never do (they would flood the answer).
fn journal_trail_line(line: &crate::journal::JournalLine) -> Option<String> {
    use crate::journal::JournalLine as J;
    match line {
        J::Open {
            risk,
            model,
            regime,
            ..
        } => Some(format!(
            "open · risk {risk}{model}{regime}",
            model = model
                .as_deref()
                .map(|m| format!(" · model {m}"))
                .unwrap_or_default(),
            regime = regime
                .as_deref()
                .map(|g| format!(" · regime {g}"))
                .unwrap_or_default(),
        )),
        J::Exit { trade, .. } => Some(format!(
            "exit {:?} · {} ({:+.1}%)",
            trade.reason,
            signed_usd(trade.net_pnl),
            trade.net_pnl_percent
        )),
        J::BuySkip { reason, .. } => Some(format!("buy skipped — {reason}")),
        J::BuyAttempt { bundle, fill, .. } => Some(format!(
            "buy attempt · bundle {bundle}{filled}",
            bundle = short_mint(bundle),
            filled = if fill.is_some() {
                " · filled"
            } else {
                " · unconfirmed"
            },
        )),
        J::BuyOutcome {
            verdict, detail, ..
        } => Some(format!(
            "buy {verdict}{detail}",
            detail = detail
                .as_deref()
                .map(|d| format!(" — {d}"))
                .unwrap_or_default(),
        )),
        J::SellAttempt {
            venue,
            tokens,
            price_usd,
            ..
        } => Some(format!("sell {venue} {tokens} tok @ {price_usd}")),
        J::SellGiveUp { cause, .. } => Some(format!("sell giveup — {cause}")),
        J::Gap {
            leg,
            gap_usd,
            venue,
            ..
        } => Some(format!(
            "{leg} gap {}{}",
            signed_usd(*gap_usd),
            if venue.is_empty() {
                String::new()
            } else {
                format!(" via {venue}")
            }
        )),
        J::Tick { .. } | J::Boot { .. } | J::Equity { .. } => None,
    }
}

/// `/token` answer (pure): where one mint stands plus its recent
/// journal trail. Absence is stated per source and per coverage
/// window, never invented: the book claims only cover lists the file
/// publishes in full, watchlist claims respect the capped sample, and
/// trail absence names the scanned span and any unreadable lines.
fn token_text(
    report: &StatusReport,
    mint: &str,
    trail: &[&crate::journal::JournalLine],
    tail: &crate::journal::JournalTail,
) -> String {
    let mut out = format!("**Token** {short}\n", short = short_mint(mint));
    let mint_ref = find_mint(report, mint);
    // Open positions publish as a capped sample: overflowed = absence
    // from the shown slice proves nothing about the open book.
    let book_sample_overflow = report.report.open_positions > report.positions.len();
    match mint_ref {
        MintRef::Open(i) => {
            let p = &report.positions[i];
            out.push_str(&format!(
                "Book: open #{id} — {qty} tok @ {entry} = {basis} · held {held}\n",
                id = p.id,
                qty = tok_count(p.quantity),
                entry = p.entry_price,
                basis = usd(p.entry_notional),
                held = dur(now_secs().saturating_sub(p.opened_at)),
            ));
        }
        MintRef::ManualBag => {
            out.push_str("Book: unpaired live bag (on-chain, outside the paper book)\n");
        }
        MintRef::Diverged => {
            out.push_str("Book: diverged (reconciling remote)\n");
        }
        MintRef::PendingBuy => {
            out.push_str("Book: pending buy (unconfirmed, watch-only)\n");
        }
        MintRef::Unknown => {
            // Only claim "not open" when the sample is complete — an
            // overflowed cap means absence here proves nothing about
            // the open book.
            if !book_sample_overflow {
                out.push_str("Book: no record (not open, bagged, diverged, or pending)\n");
            } else {
                out.push_str(&format!(
                    "Book: not in the shown sample — {o} open, {n} shown (file cap) — so absence here is not proof of absence\n",
                    o = report.report.open_positions,
                    n = report.positions.len(),
                ));
            }
        }
    }
    match report.watchlist.entries.iter().find(|e| e.mint == mint) {
        Some(e) => {
            let now = now_secs();
            let phase = if e.deadline_secs <= now {
                "deadline passed".to_string()
            } else {
                format!("expires in {}", dur(e.deadline_secs - now))
            };
            if mint_ref == MintRef::Unknown {
                // Watch pending, quorum in flight: this is the ONE
                // state a watchlist line means.
                out.push_str(&format!(
                    "Watchlist: {buyers}/{min} buyers · {phase} · audit {score}\n",
                    buyers = e.buyers,
                    min = report.watchlist.min_buyers,
                    phase = phase,
                    score = e
                        .audit_score
                        .map(|s| format!("score {s}"))
                        .unwrap_or_else(|| "unaudited so far".to_string()),
                ));
            } else {
                // Held or live-and-held mints stay on the gate's watch
                // (the watch pins while the curve is live). Quorum
                // numbers next to a book line must not read as "still
                // waiting": name the pin, not a phantom gate wait.
                out.push_str(&format!(
                    "Watchlist: still tracked while held ({buyers}/{min} buyers · {phase}) — the watch pins while the mint is held; no quorum wait\n",
                    buyers = e.buyers,
                    min = report.watchlist.min_buyers,
                    phase = phase,
                ));
            }
        }
        None if mint_ref == MintRef::Unknown
            && report.watchlist.total > report.watchlist.entries.len() =>
        {
            // Sampled watchlist: absence from the shown slice is not
            // "not watched" when the file capped the entries.
            out.push_str(&format!(
                "Watchlist: not in the shown sample — {} of {} shown (file cap), so absence here is not proof of absence\n",
                report.watchlist.entries.len(),
                report.watchlist.total,
            ));
        }
        None => {}
    }
    let shown: Vec<String> = trail
        .iter()
        .filter_map(|l| journal_trail_line(l))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .take(TOKEN_TRAIL_SHOWN)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if shown.is_empty() {
        out.push_str(&format!(
            "Trail: no journal entries for this mint in the last {scanned} lines",
            scanned = tail.scanned,
        ));
        if tail.error.is_some() {
            out.push_str(" (journal unreadable — absence here proves nothing)");
        } else {
            if tail.truncated {
                out.push_str(" (older history not scanned — absence here is not proof)");
            }
            if tail.bad > 0 {
                out.push_str(&format!(", {bad} unreadable skipped", bad = tail.bad));
            }
        }
    } else {
        out.push_str("Trail (newest last):\n");
        for line in shown {
            out.push_str(&format!("- {line}\n"));
        }
    }
    out
}

/// `on`/`off` for boolean switches in config text.
fn on_off(v: bool) -> &'static str {
    if v { "on" } else { "off" }
}

/// Knob rendering for `0 = off` money/percent settings: `off` at
/// zero, the given `fmt` otherwise (guards never block at zero).
fn off_or(v: f64, fmt: &dyn Fn(f64) -> String) -> String {
    if v == 0.0 { "off".to_string() } else { fmt(v) }
}

/// `/config` answer: reads the operator's `config.toml` through the
/// bot's own parser (single source — file typos surface here as
/// notes, exactly as the running bot logged them at boot). Endpoint
/// values are REDACTED in every line: RPC/geyser URLs carry
/// `?api-key=` / path secrets in this repo's setups, and Discord
/// chat is never a place for them. Trading knobs disclose plainly —
/// it is configuration, not custody; keys never appear in answers.
fn config_text() -> String {
    let path = crate::config::resolve_config_path();
    let (source, table) = match crate::config::read_table(&path) {
        Ok(Some(table)) => (format!("file `{}`", path.display()), Some(table)),
        Ok(None) => (
            format!("file `{}` missing — built-in defaults rule", path.display()),
            None,
        ),
        Err(e) => {
            return config_refusal(&path, &e.to_string());
        }
    };
    let s = Settings::from_file(table.as_ref());
    let kw = |set: bool| if set { "set" } else { "unset" };
    let mut out = format!("**Config** — {source}\n");
    out.push_str(&format!(
        "Strategy: `{}` · runner moonbag {:.0}% · exit regime {}\n",
        s.strategy_mode,
        s.runner_moonbag_percent,
        if s.force_exit_regime.is_empty() {
            "LLM selects".to_string()
        } else {
            format!("forced `{}`", s.force_exit_regime)
        }
    ));
    out.push_str(&format!(
        "Inference: `{}` (endpoint {}) · fallback {} · defers ≤{} · timeout {}ms\n",
        s.inference_model,
        kw(!s.inference_endpoint.is_empty()),
        s.inference_fallback,
        s.inference_defer_max_n,
        s.inference_timeout_ms,
    ));
    out.push_str(&format!(
        "Trading: paper {} per trade · max open {} · exposure cap {} · daily-loss cap {} · cooldown {}s\n",
        usd(s.notional_usd),
        s.max_open_positions,
        off_or(s.max_exposure_usd, &usd),
        off_or(s.max_daily_loss_usd, &usd),
        s.mint_cooldown_secs,
    ));
    out.push_str(&format!(
        "Risk guards: fill ≥{}% · top-holder {} · flip-cost {} · entry-gap {} · exit-gap {} · entry-avg-gap {} · hot wallet {}\n",
        s.min_fill_rate_pct,
        off_or(s.max_top_holder_pct, &signed_pct),
        off_or(s.max_entry_flip_loss_pct, &signed_pct),
        off_or(s.max_expected_entry_gap_usd, &usd),
        off_or(s.max_avg_exit_gap_usd, &usd),
        off_or(s.max_avg_entry_gap_usd, &usd),
        off_or(s.hot_wallet_max_sol, &|n: f64| format!("{n:.2} SOL")),
    ));
    out.push_str(&format!(
        "Exits: trail {}% · runner {}% · gate window {}s · gate buyers {}\n",
        s.trail_percent, s.runner_trail_percent, s.gate_window_secs, s.gate_min_buyers,
    ));
    out.push_str(&format!(
        "Execution: slippage {}bps · vol-slip {}/{} · priority {} µLamports · CU {} · jito tip {} · sim-strict {}\n",
        s.slippage_bps,
        s.slippage_vol_window_secs,
        s.slippage_vol_extra_cap_bps,
        s.priority_fee_micro_lamports,
        s.compute_unit_limit,
        s.tip_lamports,
        on_off(s.sim_strict),
    ));
    out.push_str(&format!(
        "Endpoints: ws {} · http {} · fallbacks {} · geyser {} — values REDACTED (they can carry api-key secrets)\n",
        kw(!s.ws_url.is_empty()),
        kw(!s.http_url.is_empty()),
        s.fallback_rpc_urls.len(),
        kw(!s.yellowstone_endpoint.is_empty()),
    ));
    out.push_str(&format!(
        "Alerts: cards {} (channel {}) · diverged warn ≥{}\n",
        on_off(s.discord_alerts),
        if s.discord_channel.is_some() {
            "set"
        } else {
            "unset"
        },
        s.alert_diverged_warn_n,
    ));
    if s.parse_notes.is_empty() {
        out.push_str("Notes: clean — every set key known, every value typed.");
    } else {
        out.push_str("⚠ Config notes (logged by the running bot at boot too):\n");
        for note in s.parse_notes {
            out.push_str(&format!("- {note}\n"));
        }
    }
    out.push_str("Values are the file as read just now — the running bot's copy dates to its boot; restart applies changes.");
    out
}

/// Operator-facing refusal for an unreadable `config.toml` (shared by
/// `/config` and `/risk`). The parse error itself is logged to the
/// sidecar's own log, NOT echoed to Discord: a TOML parse error quotes
/// the offending line, and an offending line can be an endpoint URL
/// (`?api-key=` / path secrets) — identical redaction by construction
/// to `/config`'s endpoint lines. The bot refuses to boot on these,
/// so the fix is an operator edit anyway.
fn config_refusal(path: &std::path::Path, error: &str) -> String {
    warn!(
        path = %path.display(),
        error,
        "config.toml unreadable; details stay in the sidecar log (error text can quote config lines)"
    );
    format!(
        "⚠ Config unreadable at `{}` — fix it and retry; the running bot refuses to boot with a broken file. Parse details go to the sidecar log only (a parse error can quote the offending line, which may carry endpoint secrets).",
        path.display()
    )
}

/// Tier 2 audit trail (doc §6): every control invocation — allowed
/// or denied, executed or cancelled — lands here plus the tracing log,
/// never only in Discord. Separate file from the bot journal by
/// decision (doc §8): the journal schema is the trading record.
#[derive(Debug, Clone, serde::Serialize)]
struct AuditEvent<'a> {
    ts: u64,
    user_id: u64,
    user_name: &'a str,
    guild_id: u64,
    command: &'a str,
    args: &'a str,
    outcome: &'a str,
}

/// Audit log path: beside the state files, resolved like every other
/// state path so both processes agree.
fn audit_path() -> std::path::PathBuf {
    let (_, log_file) = crate::cli::resolve_paths();
    crate::cli::state_file_beside_log(&log_file, AUDIT_FILE)
}

/// Append one audit line (0600, best-effort). Returns whether the line
/// landed: callers flag a missed audit loudly instead of pretending.
/// No secrets ever enter the line (mints/guild/user IDs are chain or
/// operator identifiers, never keys or tokens).
fn audit_append(event: &AuditEvent<'_>) -> bool {
    let mut line = match serde_json::to_vec(event) {
        Ok(v) => v,
        Err(e) => {
            error!(error = %e, "Tier 2 audit serialize failed");
            return false;
        }
    };
    line.push(b'\n');
    let path = audit_path();
    // Created owner-only from the first byte: the pre-chmod window at
    // HEAD (create with the umask, chmod after the write) let any
    // local user read the ids/outcomes between open and chmod. The
    // best-effort chmod survives for pre-existing files.
    let open = {
        #[allow(unused_mut)]
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        opts.open(&path)
    };
    let ok = open
        .and_then(|mut f| {
            use std::io::Write;
            f.write_all(&line)
        })
        .is_ok();
    if !ok {
        error!(path = %path.display(), "Tier 2 audit append failed");
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).is_err() {
            warn!(path = %path.display(), "Tier 2 audit chmod 0600 failed");
        }
    }
    true
}

/// Convenience: build, log, and append one Tier 2 audit event. Always
/// logs via tracing (infallible); the file append is best-effort with
/// a loud error and one immediate retry, and the return tells the
/// caller whether the line landed. Per-command failure contracts
/// differ (see the module doc): the availability direction
/// (`/sell`, `/pause`) keeps executing; the resume direction refuses.
fn audit(
    user_id: u64,
    user_name: &str,
    guild_id: u64,
    command: &str,
    args: &str,
    outcome: &str,
) -> bool {
    info!(
        user_id,
        user_name, guild_id, command, args, outcome, "Tier 2 control"
    );
    let event = AuditEvent {
        ts: now_secs(),
        user_id,
        user_name,
        guild_id,
        command,
        args,
        outcome,
    };
    if audit_append(&event) {
        return true;
    }
    // Retry immediately once; the failure stays loud either way so an
    // operator can scrape it. Two failures means the audit dir is
    // broken (disk full, permissions), not a transient hiccup.
    error!(command, "Tier 2 audit append failed; retrying once");
    audit_append(&event)
}

/// A control action awaiting its button click. Memory-only: a restart
/// drops every pending confirm (unknown custom_id = re-issue).
#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingAction {
    /// Queue a sell-request file for this mint on Confirm.
    Sell { mint: String },
    /// Clear the pause marker on Confirm.
    Resume,
}

#[derive(Debug, Clone)]
struct PendingConfirm {
    action: PendingAction,
    /// Requester user ID (audited; any operator may click).
    requester: u64,
    /// Unix seconds at issue; clicks past TTL are expired no-ops.
    created: u64,
}

/// Button custom_id wire shape: `ps2:<nonce>:<confirm|cancel>`.
/// The nonce is the whole key (the action rides server-side, never in
/// the button — a forged custom_id matches no pending entry).
fn confirm_custom_id(nonce: &str, verb: &str) -> String {
    format!("ps2:{nonce}:{verb}")
}

/// Parse a button custom_id back into (nonce, verb).
fn parse_confirm_id(custom_id: &str) -> Option<(&str, &str)> {
    let rest = custom_id.strip_prefix("ps2:")?;
    let (nonce, verb) = rest.rsplit_once(':')?;
    if nonce.is_empty() || (verb != "confirm" && verb != "cancel") {
        return None;
    }
    Some((nonce, verb))
}

/// True when the pending confirm outlived its click window.
fn confirm_expired(created: u64, now: u64) -> bool {
    now.saturating_sub(created) >= CONFIRM_TTL_SECS
}

/// Where a mint lives in the status file, for the `/sell` confirm
/// detail and the unknown-mint refusal (doc §6: unknown mint = no
/// action, stated explicitly).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MintRef {
    /// Open paper position (index into `report.positions`).
    Open(usize),
    ManualBag,
    Diverged,
    PendingBuy,
    Unknown,
}

/// Classify a mint against the trailing file. Open positions first
/// (the confirm shows entry/mark detail); every other list is a
/// watch-only disclosure, never a sell target detail.
fn find_mint(report: &StatusReport, mint: &str) -> MintRef {
    if let Some(i) = report.positions.iter().position(|p| p.mint == mint) {
        return MintRef::Open(i);
    }
    if report.manual_mints.iter().any(|m| m == mint) {
        return MintRef::ManualBag;
    }
    if report.diverged_mints.iter().any(|m| m == mint) {
        return MintRef::Diverged;
    }
    if report.pending_buy_mints.iter().any(|m| m == mint) {
        return MintRef::PendingBuy;
    }
    MintRef::Unknown
}

/// Mint argument of a `/sell` invocation (the single required string
/// option). `None` when absent or mistyped (Discord enforces the
/// shape; this is the fail-closed fallback).
fn sell_mint_arg(cmd: &CommandInteraction) -> Option<String> {
    string_option(cmd, "mint")
}

/// Mint argument of a `/token` invocation. Same shape contract as
/// `/sell` (one required `mint` string), separate name so the two
/// commands never share parsing.
fn token_mint_arg(cmd: &CommandInteraction) -> Option<String> {
    string_option(cmd, "mint")
}

/// One required string option by name. `None` when absent or
/// mistyped (Discord enforces the shape; this is the fail-closed
/// fallback).
fn string_option(cmd: &CommandInteraction, name: &str) -> Option<String> {
    cmd.data.options.iter().find_map(|o| match &o.value {
        CommandDataOptionValue::String(s) => {
            if o.name == name {
                Some(s.clone())
            } else {
                None
            }
        }
        _ => None,
    })
}

/// Confirm/cancel button row for a pending Tier 2 action. Ephemeral
/// prompts carry their own row; the custom_ids key server-side state.
fn confirm_row(nonce: &str) -> Vec<CreateActionRow> {
    vec![CreateActionRow::Buttons(vec![
        CreateButton::new(confirm_custom_id(nonce, "confirm"))
            .label("Confirm")
            .style(ButtonStyle::Danger),
        CreateButton::new(confirm_custom_id(nonce, "cancel"))
            .label("Cancel")
            .style(ButtonStyle::Secondary),
    ])]
}

/// The sidecar's answer to one interaction (payload plus whether the
/// response must be ephemeral — explicit no-data / wrong-guild cases).
struct Answer {
    message: CreateInteractionResponseMessage,
}

/// Event handler: gateway connection + command dispatch.
struct Sidecar {
    /// The one guild the sidecar answers in (guild-locked, doc §6).
    guild_id: GuildId,
    /// Allowlisted Tier 2 operators (user IDs from `discord.toml`).
    operators: Vec<u64>,
    /// Outstanding confirm buttons (nonce → pending action).
    pending: Arc<Mutex<HashMap<String, PendingConfirm>>>,
    /// Nonce source for confirm buttons.
    nonce: AtomicU64,
}

impl Sidecar {
    /// Tier 2 gate: allowlisted operator? Deny text is ephemeral and
    /// audited — a non-operator learns nothing except the refusal.
    fn operator_check(
        &self,
        user_id: u64,
        user_name: &str,
        command: &str,
        args: &str,
    ) -> Option<Answer> {
        if is_operator(&self.operators, user_id) {
            return None;
        }
        audit(
            user_id,
            user_name,
            self.guild_id.get(),
            command,
            args,
            "denied_not_operator",
        );
        Some(Answer {
            message: CreateInteractionResponseMessage::new().ephemeral(true).embeds(vec![
                reply_embed(
                    "Tier 2 control is allowlisted: your Discord user ID is not in `operator_ids` (`discord.toml`). No action taken.",
                ),
            ]),
        })
    }

    /// Shared `/sell` gate: fresh book, running daemon, and a known
    /// mint. Runs at arm time AND again at Confirm time — up to
    /// `CONFIRM_TTL_SECS` elapse between the two, and arm validation
    /// is not execution (TOCTOU): the click re-checks the same facts
    /// before anything is queued. The error carries the operator text
    /// plus an audit outcome tag.
    fn sell_gate(&self) -> Result<StatusReport, (String, &'static str)> {
        let report = match status::read(&status::status_path()) {
            Ok(report) => report,
            Err(re) => {
                return Err((no_status_text(&re.to_string()), "denied_no_status"));
            }
        };
        let (pid_file, _) = crate::cli::resolve_paths();
        let daemon = crate::cli::running_daemon(&pid_file).is_some();
        if let Some(text) = sell_gate_text(report.ts, now_secs(), daemon) {
            let tag = if text.contains("Status is") {
                "denied_stale_status"
            } else {
                "denied_no_daemon"
            };
            return Err((text, tag));
        }
        Ok(report)
    }

    /// Reap expired confirm entries (every issue and every click use
    /// this; a timed task in `run` reaps without any interaction).
    fn sweep_expired_confirms(&self) {
        self.pending
            .lock()
            .expect("pending lock")
            .retain(|_, p| !confirm_expired(p.created, now_secs()));
    }

    /// Issue a confirm prompt: stores the pending action, returns the
    /// ephemeral message with Confirm/Cancel buttons.
    fn issue_confirm(&self, action: PendingAction, detail: String, user_id: u64) -> Answer {
        // Reap expired entries on every issue: a long-lived sidecar
        // whose buttons are never clicked must not grow the map.
        self.sweep_expired_confirms();
        let nonce = format!(
            "{:x}{:x}",
            now_secs(),
            self.nonce.fetch_add(1, Ordering::Relaxed)
        );
        {
            let mut map = self.pending.lock().expect("pending lock");
            if map.len() >= PENDING_CONFIRM_CAP {
                warn!(
                    cap = PENDING_CONFIRM_CAP,
                    "confirm button refused: outstanding-confirm cap reached"
                );
                return Answer {
                    message: CreateInteractionResponseMessage::new().ephemeral(true).embeds(
                        vec![reply_embed(&format!(
                            "{detail}\n⚠ Confirm queue is full ({PENDING_CONFIRM_CAP} outstanding) — cancel or wait out an outstanding confirm first. No action armed."
                        ))],
                    ),
                };
            }
            map.insert(
                nonce.clone(),
                PendingConfirm {
                    action,
                    requester: user_id,
                    created: now_secs(),
                },
            );
        }
        let _ = user_id;
        Answer {
            message: CreateInteractionResponseMessage::new()
                .ephemeral(true)
                .embeds(vec![reply_embed(&format!(
                    "{detail}\nConfirm within {}m — no confirm = no-op.",
                    CONFIRM_TTL_SECS / 60
                ))])
                .components(confirm_row(&nonce)),
        }
    }

    /// `/sell <mint>`: validate, show position detail, arm Confirm.
    /// Nothing is written here — the file lands only on the button.
    fn render_sell(&self, cmd: &CommandInteraction) -> Answer {
        let user_id = cmd.user.id.get();
        let user_name = cmd.user.name.clone();
        let Some(mint) = sell_mint_arg(cmd) else {
            audit(
                user_id,
                &user_name,
                self.guild_id.get(),
                SELL_COMMAND,
                "",
                "denied_no_mint",
            );
            return Answer {
                message: CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .embeds(vec![reply_embed(
                        "`/sell` needs a mint: `/sell mint:<address>`. No action taken.",
                    )]),
            };
        };
        if let Some(deny) = self.operator_check(user_id, &user_name, SELL_COMMAND, &mint) {
            return deny;
        }
        if executor::parse_pubkey(&mint).is_err() {
            audit(
                user_id,
                &user_name,
                self.guild_id.get(),
                SELL_COMMAND,
                &mint,
                "denied_bad_mint",
            );
            return Answer {
                message: CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .embeds(vec![reply_embed(&format!(
                        "`{mint}` is not a base58 pubkey. No action taken."
                    ))]),
            };
        }
        let report = match self.sell_gate() {
            Ok(report) => report,
            Err((text, tag)) => {
                audit(
                    user_id,
                    &user_name,
                    self.guild_id.get(),
                    SELL_COMMAND,
                    &mint,
                    tag,
                );
                return Answer {
                    message: CreateInteractionResponseMessage::new()
                        .ephemeral(true)
                        .embeds(vec![reply_embed(&text)]),
                };
            }
        };
        let detail = match find_mint(&report, &mint) {
            MintRef::Open(i) => {
                let p = &report.positions[i];
                let held = now_secs().saturating_sub(p.opened_at);
                let mark_part = match (p.mark_price, p.unrealized_usd) {
                    (Some(mark), Some(unreal)) => {
                        format!("mark {mark} ({})", signed_usd(unreal))
                    }
                    _ => "no fresh mark — held at basis".to_string(),
                };
                format!(
                    "Sell confirm — #{id} {mint} — {qty} tok @ {entry} = {basis} · {mark_part} · held {held}",
                    id = p.id,
                    mint = short_mint(&p.mint),
                    qty = tok_count(p.quantity),
                    entry = p.entry_price,
                    basis = usd(p.entry_notional),
                    held = dur(held),
                )
            }
            MintRef::ManualBag => format!(
                "Sell confirm — {} is an unpaired live bag (on-chain, outside the paper book). Queuing the request file; the daemon drains what it holds.",
                short_mint(&mint)
            ),
            MintRef::Diverged => format!(
                "Sell confirm — {} is diverged (reconciling remote). Queuing the request file; the daemon drains what it holds.",
                short_mint(&mint)
            ),
            MintRef::PendingBuy => format!(
                "Sell confirm — {} has an unsettled buy awaiting confirmation. Queuing the request file; the daemon drains what it holds.",
                short_mint(&mint)
            ),
            MintRef::Unknown => {
                audit(
                    user_id,
                    &user_name,
                    self.guild_id.get(),
                    SELL_COMMAND,
                    &mint,
                    "denied_unknown_mint",
                );
                return Answer {
                    message: CreateInteractionResponseMessage::new().ephemeral(true).embeds(
                        vec![reply_embed(&format!(
                            "`{}` is in no book list (open, bags, diverged, pending). Unknown mint: no action taken.",
                            short_mint(&mint)
                        ))],
                    ),
                };
            }
        };
        audit(
            user_id,
            &user_name,
            self.guild_id.get(),
            SELL_COMMAND,
            &mint,
            "confirm_armed",
        );
        self.issue_confirm(PendingAction::Sell { mint }, detail, user_id)
    }

    /// `/pause`: safe direction — no confirm. Writes the same marker
    /// bytes as CLI `pause`; idempotent (already paused = honest note).
    fn render_pause(&self, cmd: &CommandInteraction) -> Answer {
        let user_id = cmd.user.id.get();
        let user_name = cmd.user.name.clone();
        if let Some(deny) = self.operator_check(user_id, &user_name, PAUSE_COMMAND, "") {
            return deny;
        }
        let text = self.pause_handle(user_id, &user_name, self.guild_id.get());
        Answer {
            message: CreateInteractionResponseMessage::new()
                .ephemeral(true)
                .embeds(vec![reply_embed(&text)]),
        }
    }

    /// `/pause` execution: the pause lands first (exit availability),
    /// then the audit line (retried once); a failed audit only warns —
    /// pausing must never depend on auditability.
    fn pause_handle(&self, user_id: u64, user_name: &str, guild_id: u64) -> String {
        let path = crate::cli::entries_paused_path();
        let already = crate::cli::marker_present(&path);
        match crate::cli::set_entries_paused_at(&path) {
            Ok(_) => {
                let outcome = if already {
                    "executed_already_paused"
                } else {
                    "executed"
                };
                let landed = audit(user_id, user_name, guild_id, PAUSE_COMMAND, "", outcome);
                let mut text = if already {
                    "Entries paused (was already paused — marker timestamp refreshed). Open positions keep managing."
                } else {
                    "Entries paused: no new position opens; open positions keep managing (exits, `sell <mint>`). Resume with `/resume`."
                }
                .to_string();
                if !landed {
                    text.push_str(
                        "\n⚠ Audit append failed — check the sidecar log; the pause itself landed.",
                    );
                }
                text
            }
            Err(e) => {
                audit(
                    user_id,
                    user_name,
                    guild_id,
                    PAUSE_COMMAND,
                    "",
                    "error_marker_write",
                );
                format!("Pause marker write failed ({e}). Entries NOT paused.")
            }
        }
    }

    /// `/resume`: unsafe direction — arms Confirm when paused; honest
    /// no-op when already clear.
    fn render_resume(&self, cmd: &CommandInteraction) -> Answer {
        let user_id = cmd.user.id.get();
        let user_name = cmd.user.name.clone();
        if let Some(deny) = self.operator_check(user_id, &user_name, RESUME_COMMAND, "") {
            return deny;
        }
        if !crate::cli::marker_present(&crate::cli::entries_paused_path()) {
            audit(
                user_id,
                &user_name,
                self.guild_id.get(),
                RESUME_COMMAND,
                "",
                "noop_not_paused",
            );
            return Answer {
                message: CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .embeds(vec![reply_embed(
                        "Entries were not paused; nothing to resume.",
                    )]),
            };
        }
        audit(
            user_id,
            &user_name,
            self.guild_id.get(),
            RESUME_COMMAND,
            "",
            "confirm_armed",
        );
        self.issue_confirm(
            PendingAction::Resume,
            "Resume confirm — clearing the pause marker re-enables new entries.".to_string(),
            user_id,
        )
    }

    /// Button click: resolve the pending action (expiry sweep first),
    /// enforce operator-ness of the clicker, execute or cancel.
    /// Answers by editing the prompt message in place.
    async fn render_component(&self, ctx: &Context, comp: &ComponentInteraction) {
        let user_id = comp.user.id.get();
        let user_name = comp.user.name.clone();
        let guild_id = comp.guild_id.map(|g| g.get()).unwrap_or(0);
        // Expired entries die here (lazy sweep: every click reaps).
        self.sweep_expired_confirms();
        let outcome_text = match parse_confirm_id(&comp.data.custom_id) {
            None => {
                audit(
                    user_id,
                    &user_name,
                    guild_id,
                    "button",
                    &comp.data.custom_id,
                    "denied_unknown_button",
                );
                "Unknown button (restarted sidecar, or forged custom_id). Re-issue the command. No action taken.".to_string()
            }
            Some((nonce, verb)) => {
                let pending = self.pending.lock().expect("pending lock").remove(nonce);
                match (pending, verb) {
                    (None, _) => {
                        audit(
                            user_id,
                            &user_name,
                            guild_id,
                            "button",
                            &comp.data.custom_id,
                            "expired",
                        );
                        "Confirm expired (or already handled) — re-issue the command. No action taken.".to_string()
                    }
                    (Some(p), "cancel") => {
                        let cmd = match &p.action {
                            PendingAction::Sell { .. } => SELL_COMMAND,
                            PendingAction::Resume => RESUME_COMMAND,
                        };
                        audit(user_id, &user_name, guild_id, cmd, "", "cancelled");
                        "Cancelled. No action taken.".to_string()
                    }
                    (Some(p), _) => {
                        if !is_operator(&self.operators, user_id) {
                            let cmd = match &p.action {
                                PendingAction::Sell { .. } => SELL_COMMAND,
                                PendingAction::Resume => RESUME_COMMAND,
                            };
                            audit(
                                user_id,
                                &user_name,
                                guild_id,
                                cmd,
                                "",
                                "denied_not_operator",
                            );
                            // Put it back: a non-operator's click must
                            // not burn the requester's pending confirm.
                            self.pending
                                .lock()
                                .expect("pending lock")
                                .insert(nonce.to_string(), p);
                            "Tier 2 control is allowlisted: your Discord user ID is not in `operator_ids`. No action taken.".to_string()
                        } else {
                            let requester = p.requester;
                            self.execute_confirmed(
                                user_id, &user_name, guild_id, requester, p.action,
                            )
                            .await
                        }
                    }
                }
            }
        };
        let response = CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new()
                .embeds(vec![reply_embed(&outcome_text)])
                .components(vec![]),
        );
        if let Err(e) = comp.create_response(&ctx.http, response).await {
            warn!(error = %e, "Discord component response failed");
        }
    }

    /// Runs the confirmed action (files only — the sidecar never signs
    /// or dispatches) and returns the outcome text for the prompt edit.
    async fn execute_confirmed(
        &self,
        user_id: u64,
        user_name: &str,
        guild_id: u64,
        requester: u64,
        action: PendingAction,
    ) -> String {
        match action {
            PendingAction::Sell { mint } => {
                // Click-time re-validation: up to CONFIRM_TTL_SECS
                // (5 min) elapse between arm and confirm. The book may
                // have gone stale, the daemon may have stopped, and
                // the mint may have left the lists in that window.
                // Arm validation is not execution (TOCTOU) — re-run
                // the same gate before queuing anything.
                let report = match self.sell_gate() {
                    Ok(report) => report,
                    Err((text, tag)) => {
                        let confirm_tag = tag.replacen("denied_", "confirm_denied_", 1);
                        audit(
                            user_id,
                            user_name,
                            guild_id,
                            SELL_COMMAND,
                            &mint,
                            &confirm_tag,
                        );
                        return format!("Sell confirm lapsed: {text}");
                    }
                };
                if matches!(find_mint(&report, &mint), MintRef::Unknown) {
                    audit(
                        user_id,
                        user_name,
                        guild_id,
                        SELL_COMMAND,
                        &mint,
                        "denied_unknown_mint",
                    );
                    return format!(
                        "`{}` is in no book list (open, bags, diverged, pending). Unknown mint: no action taken.",
                        short_mint(&mint)
                    );
                }
                let (_, log_file) = crate::cli::resolve_paths();
                match crate::cli::queue_sell_request_at(&log_file, &mint) {
                    Ok(path) => {
                        let args = format!("{mint} (requested by {requester})");
                        let landed = audit(
                            user_id,
                            user_name,
                            guild_id,
                            SELL_COMMAND,
                            &args,
                            "executed",
                        );
                        let mut text = format!(
                            "Sell queued for {} (daemon drains it on its next round; file {}).",
                            short_mint(&mint),
                            path.display()
                        );
                        if !landed {
                            text.push_str(" ⚠ Audit append failed — check the sidecar log; the request itself landed.");
                        }
                        text
                    }
                    Err(e) => {
                        audit(
                            user_id,
                            user_name,
                            guild_id,
                            SELL_COMMAND,
                            &mint,
                            "error_queue_write",
                        );
                        format!("Sell-request write failed ({e}). No action taken.")
                    }
                }
            }
            PendingAction::Resume => {
                // Resume is the unsafe direction, so auditability is
                // load-bearing: the line is written (and retried)
                // BEFORE the marker clears — no durable audit record,
                // no resume. A failed clear after a landed line stays
                // honest via the follow-up audit outcome below.
                let args = format!("requested by {requester}");
                if !audit(
                    user_id,
                    user_name,
                    guild_id,
                    RESUME_COMMAND,
                    &args,
                    "executing",
                ) {
                    return "Audit write failed — resume REFUSED (no audit record, no resume). Fix the audit file's directory and retry `/resume`.".to_string();
                }
                match crate::cli::clear_entries_paused_at(&crate::cli::entries_paused_path()) {
                    Ok(true) => {
                        let landed = audit(
                            user_id,
                            user_name,
                            guild_id,
                            RESUME_COMMAND,
                            &args,
                            "executed",
                        );
                        let mut text = String::from("Entries resumed: new entries may open again.");
                        if !landed {
                            text.push_str(" ⚠ Audit append failed — check the sidecar log; the marker itself cleared.");
                        }
                        text
                    }
                    Ok(false) => {
                        audit(
                            user_id,
                            user_name,
                            guild_id,
                            RESUME_COMMAND,
                            "",
                            "noop_already_clear",
                        );
                        "Marker was already clear (a concurrent resume won). Nothing to resume."
                            .to_string()
                    }
                    Err(e) => {
                        audit(
                            user_id,
                            user_name,
                            guild_id,
                            RESUME_COMMAND,
                            "",
                            "error_marker_clear",
                        );
                        format!("Pause-marker clear failed ({e}). Entries still paused.")
                    }
                }
            }
        }
    }
}

impl Sidecar {
    /// Guild gate first: anything outside the configured server is
    /// denied before any file is read (fail closed).
    fn guild_check(&self, cmd: &CommandInteraction) -> Option<Answer> {
        if cmd.guild_id.is_none_or(|g| g != self.guild_id) {
            return Some(Answer {
                message: CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .embeds(vec![reply_embed(
                        "This sidecar answers only in its configured server. Nothing disclosed.",
                    )]),
            });
        }
        None
    }

    /// `/status`: compact view from on-disk state; unreadable/old
    /// states answer honestly. Stale-but-readable still shows (with a
    /// loud STALE marker) — an operator checking on a dead bot wants
    /// the last readings.
    fn render_status(&self, now: u64) -> Answer {
        let fresh = |report: &StatusReport| now.saturating_sub(report.ts) <= STALE_WARN_SECS;
        match status::read(&status::status_path()) {
            Ok(report) if fresh(&report) => Answer {
                message: CreateInteractionResponseMessage::new().ephemeral(true).embeds(
                    // Operator-only: the halted-breaker line can name
                    // the day figure, which the project keeps out of
                    // public channels (see the `/pnl` rule).
                    vec![reply_embed(&status_compact_text(&report, now))],
                ),
            },
            Ok(report) => Answer {
                message: CreateInteractionResponseMessage::new().ephemeral(true).embeds(
                    vec![reply_embed(&format!(
                        "⚠ Status is {}s old (limit {STALE_WARN_SECS}s) — bot may be down. Last readings:\n\n{}",
                        now.saturating_sub(report.ts),
                        status_compact_text(&report, now)
                    ))],
                ),
            },
            Err(re) => Answer {
                message: CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .embeds(vec![reply_embed(&no_status_text(&re.to_string()))]),
            },
        }
    }

    /// `/heartbeat`: the full production card from on-disk state.
    /// Ephemeral: the card carries the ledger view (cash/equity, and
    /// the halted breaker's day figure), which stays operator-only
    /// exactly like `/pnl`.
    fn render_heartbeat(&self, now: u64) -> Answer {
        match self.read_report() {
            Ok(report) => {
                // Embed-only, data lives in the structured fields: the
                // card's text view is a full mirror of those fields, so
                // the description carries only the additive notes
                // (freshness line, tally caveat), never the mirror.
                let card = heartbeat_card(&report, now);
                let mut embed = embed_from(&card.embed);
                embed = embed.description(alert::discord::truncate_description(&heartbeat_notes(
                    &report, now,
                )));
                Answer {
                    message: CreateInteractionResponseMessage::new()
                        .ephemeral(true)
                        .embeds(vec![embed]),
                }
            }
            Err(re) => Answer {
                message: CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .embeds(vec![reply_embed(&no_status_text(&re.to_string()))]),
            },
        }
    }

    /// Read + parse the status file.
    fn read_report(&self) -> std::io::Result<StatusReport> {
        status::read(&status::status_path())
    }

    /// The no-data failure as the operator-facing answer (fail
    /// closed, ephemeral).
    fn no_data(&self, re: &std::io::Error) -> Answer {
        Answer {
            message: CreateInteractionResponseMessage::new()
                .ephemeral(true)
                .embeds(vec![reply_embed(&no_status_text(&re.to_string()))]),
        }
    }

    /// `/positions`: open paper lines plus unpaired live custody.
    /// Ephemeral: entry sizes and PnL are ledger data.
    fn render_positions(&self, now: u64) -> Answer {
        match self.read_report() {
            Ok(report) => Answer {
                message: CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .embeds(vec![reply_embed(&with_freshness(
                        positions_text(&report, now),
                        &report,
                        now,
                    ))]),
            },
            Err(re) => self.no_data(&re),
        }
    }

    /// `/pnl`: day net, book lifetime, open book, cash. The whole
    /// ledger panel discloses — hence ephemeral.
    fn render_pnl(&self, now: u64) -> Answer {
        match self.read_report() {
            Ok(report) => Answer {
                message: CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .embeds(vec![reply_embed(&with_freshness(
                        pnl_text(&report),
                        &report,
                        now,
                    ))]),
            },
            Err(re) => self.no_data(&re),
        }
    }

    /// `/watchlist`: public (mints are chain data, buyers anonymous).
    fn render_watchlist(&self, now: u64) -> Answer {
        match self.read_report() {
            Ok(report) => Answer {
                message: CreateInteractionResponseMessage::new()
                    .embeds(vec![reply_embed(&watchlist_text(&report, now))]),
            },
            Err(re) => self.no_data(&re),
        }
    }

    /// `/config`: the operator's own file, endpoints redacted.
    fn render_config(&self) -> Answer {
        Answer {
            message: CreateInteractionResponseMessage::new()
                .embeds(vec![reply_embed(&config_text())]),
        }
    }

    /// `/research`: offline paper-pattern summary from the journal
    /// tail. Ephemeral (money-adjacent) and read-only; the journal is
    /// a live file, so staleness does not apply — coverage is stated.
    fn render_research(&self) -> Answer {
        let tail = crate::journal::load_recent_lines(
            &crate::journal::default_journal_path(),
            RESEARCH_JOURNAL_TAIL,
            RESEARCH_JOURNAL_MAX_BYTES,
        );
        let text = match tail.error {
            Some(e) => format!(
                "⚠ Journal unreadable ({e}) — an unreadable journal is not evidence of zero decisions. Nothing to report."
            ),
            None if tail.lines.is_empty() => {
                let mut text = "No decisions recorded yet — the bot has not journaled any entry signal. Nothing to analyze.".to_string();
                if tail.truncated {
                    text.push_str(" (read bounded to a recent tail; older history not scanned)");
                }
                text
            }
            None => {
                let m = crate::replay::metrics(&tail.lines);
                match crate::research::summarize(&m.micro_rows) {
                    Some(s) => research_text(&s, &tail),
                    None => "No signaled exits yet — opens exist but nothing closed against an entry signal.".to_string(),
                }
            }
        };
        Answer {
            message: CreateInteractionResponseMessage::new()
                .ephemeral(true)
                .embeds(vec![reply_embed(&text)]),
        }
    }

    /// `/risk`: limit utilization from config caps against live usage.
    /// Ephemeral: posture data. Caps read from `config.toml` as read
    /// just now (same parser as `/config`); usage rides `status.json`.
    fn render_risk(&self, now: u64) -> Answer {
        let path = crate::config::resolve_config_path();
        let settings = match crate::config::read_table(&path) {
            Ok(table) => Settings::from_file(table.as_ref()),
            Err(e) => {
                return Answer {
                    message: CreateInteractionResponseMessage::new()
                        .ephemeral(true)
                        .embeds(vec![reply_embed(&config_refusal(&path, &e.to_string()))]),
                };
            }
        };
        match self.read_report() {
            Ok(report) => Answer {
                message: CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .embeds(vec![reply_embed(&with_freshness(
                        risk_text(&settings, &report),
                        &report,
                        now,
                    ))]),
            },
            Err(re) => self.no_data(&re),
        }
    }

    /// `/token <mint>`: where one mint stands — book, watchlist (with
    /// audit score), custody lists — plus its recent journal trail.
    /// Ephemeral: per-mint decision history. Unknown mints answer
    /// honestly ("no record"), never as fiction.
    fn render_token(&self, cmd: &CommandInteraction, now: u64) -> Answer {
        let Some(mint) = token_mint_arg(cmd) else {
            return Answer {
                message: CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .embeds(vec![reply_embed(
                        "`/token` needs a mint: `/token mint:<address>`. No action taken.",
                    )]),
            };
        };
        if executor::parse_pubkey(&mint).is_err() {
            return Answer {
                message: CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .embeds(vec![reply_embed(&format!(
                        "`{mint}` is not a base58 pubkey. No action taken."
                    ))]),
            };
        }
        match self.read_report() {
            Ok(report) => {
                let tail = crate::journal::load_recent_lines(
                    &crate::journal::default_journal_path(),
                    TOKEN_JOURNAL_TAIL,
                    TOKEN_JOURNAL_MAX_BYTES,
                );
                let trail: Vec<&crate::journal::JournalLine> = tail
                    .lines
                    .iter()
                    .filter(|l| journal_mint(l) == Some(mint.as_str()))
                    .collect();
                let text = token_text(&report, &mint, &trail, &tail);
                Answer {
                    message: CreateInteractionResponseMessage::new()
                        .ephemeral(true)
                        .embeds(vec![reply_embed(&with_freshness(text, &report, now))]),
                }
            }
            Err(re) => self.no_data(&re),
        }
    }
}

#[async_trait]
impl EventHandler for Sidecar {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!(user = %ready.user.name, "Discord sidecar connected");
        let commands = vec![
            CreateCommand::new(STATUS_COMMAND)
                .description("Bot status: mode, uptime, breaker, data health"),
            CreateCommand::new(HEARTBEAT_COMMAND)
                .description("Full heartbeat card: counters, RPC pool, stream health"),
            CreateCommand::new(POSITIONS_COMMAND)
                .description("Open paper positions: entries, marks, PnL (private reply)"),
            CreateCommand::new(PNL_COMMAND)
                .description("Ledger panel: day net, realized/unrealized, cash (private reply)"),
            CreateCommand::new(WATCHLIST_COMMAND)
                .description("Buyer-quorum watchlist: mints, buyers, deadlines"),
            CreateCommand::new(CONFIG_COMMAND)
                .description("Effective trading config from config.toml (endpoints redacted)"),
            CreateCommand::new(RESEARCH_COMMAND)
                .description("Offline paper-pattern summary from the journal (private reply)"),
            CreateCommand::new(RISK_COMMAND)
                .description("Limit utilization: caps vs live usage (private reply)"),
            CreateCommand::new(TOKEN_COMMAND)
                .description("One mint: book state, audit score, journal trail (private reply)")
                .add_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "mint",
                        "Mint address to inspect",
                    )
                    .required(true),
                ),
            CreateCommand::new(SELL_COMMAND)
                .description("Queue a manual full exit for one mint (operators only, confirms)")
                .add_option(
                    CreateCommandOption::new(
                        CommandOptionType::String,
                        "mint",
                        "Mint address to exit fully",
                    )
                    .required(true),
                ),
            CreateCommand::new(PAUSE_COMMAND)
                .description("Kill switch: halt new entries (operators only, no confirm)"),
            CreateCommand::new(RESUME_COMMAND)
                .description("Clear the kill switch (operators only, confirms)"),
        ];
        match self.guild_id.set_commands(&ctx.http, commands).await {
            Ok(registered) => {
                info!(
                    n = registered.len(),
                    guild = self.guild_id.get(),
                    "guild slash commands registered"
                );
            }
            Err(e) => {
                // Loud: a failed registration means a blind sidecar.
                error!(
                    guild = self.guild_id.get(),
                    error = %e,
                    "guild slash command registration failed (bot invited to the server? correct guild id?)"
                );
            }
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Command(cmd) => {
                // Guild gate first for Tier 2 as well: nothing control
                // runs before the server check (fail closed).
                if let Some(deny) = self.guild_check(&cmd) {
                    let response = CreateInteractionResponse::Message(deny.message);
                    if let Err(e) = cmd.create_response(&ctx.http, response).await {
                        warn!(
                            command = %cmd.data.name,
                            error = %e,
                            "Discord interaction response failed"
                        );
                    }
                    return;
                }
                let dispatched = match cmd.data.name.as_str() {
                    STATUS_COMMAND => self.render_status(now_secs()),
                    HEARTBEAT_COMMAND => self.render_heartbeat(now_secs()),
                    POSITIONS_COMMAND => self.render_positions(now_secs()),
                    PNL_COMMAND => self.render_pnl(now_secs()),
                    WATCHLIST_COMMAND => self.render_watchlist(now_secs()),
                    CONFIG_COMMAND => self.render_config(),
                    RESEARCH_COMMAND => self.render_research(),
                    RISK_COMMAND => self.render_risk(now_secs()),
                    TOKEN_COMMAND => self.render_token(&cmd, now_secs()),
                    SELL_COMMAND => self.render_sell(&cmd),
                    PAUSE_COMMAND => self.render_pause(&cmd),
                    RESUME_COMMAND => self.render_resume(&cmd),
                    other => {
                        warn!(command = other, "ignoring unknown command for this sidecar");
                        return;
                    }
                };
                let response = CreateInteractionResponse::Message(dispatched.message);
                if let Err(e) = cmd.create_response(&ctx.http, response).await {
                    warn!(
                        command = %cmd.data.name,
                        error = %e,
                        "Discord interaction response failed"
                    );
                }
            }
            Interaction::Component(comp) => {
                // Buttons are Tier 2 only: guild gate, then the pending
                // lookup enforces its own operator check (a non-operator
                // click never burns the requester's confirm).
                if comp.guild_id.is_none_or(|g| g != self.guild_id) {
                    return;
                }
                self.render_component(&ctx, &comp).await;
            }
            _ => {}
        }
    }
}

/// The sidecar's only in-process storage is the confirm-button map:
/// reap expired entries on a timer, independent of any interaction,
/// so unclicked confirms never outgrow the cap by lapping the TTL.
async fn sweep_pending_confirms(pending: Arc<Mutex<HashMap<String, PendingConfirm>>>) {
    let mut every = tokio::time::interval(Duration::from_secs(CONFIRM_SWEEP_INTERVAL_SECS));
    loop {
        every.tick().await;
        pending
            .lock()
            .expect("pending lock")
            .retain(|_, p| !confirm_expired(p.created, now_secs()));
    }
}

/// Capture the token (sync, pre-thread), load the config, connect.
/// Boot errors are fail-stop: an operator wants the sidecar refusing
/// to half-run, not a phantom gateway connection.
pub fn run(token: String) -> Result<(), String> {
    // One gateway per state dir, in both modes: the pid file is written
    // after spawn by the detaching parent (and never by `--foreground`),
    // so it cannot serialize instances. The lock refuses a second
    // sidecar before any gateway connection or confirm-map state exists.
    let lock_path = crate::cli::discord_lock_path();
    let _lock = crate::cli::DiscordInstanceLock::try_acquire(&lock_path)
        .map_err(|e| format!("refusing to start a second discord sidecar: {e}"))?;
    // Register this process so `--status`/`--stop` see a foreground
    // sidecar too; the detaching parent writes the same pid afterwards.
    let pid_file = crate::cli::discord_pid_path();
    crate::cli::register_running(false, &pid_file);
    let result = run_gateway(token);
    crate::cli::unregister_running(&pid_file);
    result
}

/// Connect and serve. Never call directly: [`run`] owns the instance
/// lock and the pid registration.
fn run_gateway(token: String) -> Result<(), String> {
    let (config_path, cfg) = load_config()?;
    let guild_id = GuildId::new(cfg.guild_id);
    let pending = Arc::new(Mutex::new(HashMap::new()));
    let sidecar = Sidecar {
        guild_id,
        operators: cfg.operator_ids.clone(),
        pending: Arc::clone(&pending),
        nonce: AtomicU64::new(0),
    };
    info!(
        guild = cfg.guild_id,
        operators = cfg.operator_ids.len(),
        config = %config_path.display(),
        "Discord sidecar starting (Tier 1 read-only + Tier 2 allowlisted control)"
    );

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|e| format!("sidecar runtime: {e}"))?;
    rt.block_on(async move {
        tokio::spawn(sweep_pending_confirms(pending));
        // No message content, no presence: slash commands plus confirm
        // buttons need no privileged intents at all.
        let mut client = Client::builder(token, GatewayIntents::empty())
            .event_handler(sidecar)
            .await
            .map_err(|e| format!("Discord gateway connection: {e}"))?;
        client
            .start()
            .await
            .map_err(|e| format!("Discord gateway session: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::JournalTail;

    fn fixture_report(ts: u64) -> StatusReport {
        StatusReport {
            ts,
            report: alert::HeartbeatReport {
                mode: "paper".to_string(),
                strategy: "adaptive".to_string(),
                uptime_secs: 7_500,
                logs_seen: 1000,
                candidates: 50,
                parse_ok: 48,
                dropped: 2,
                watchlist_pending: 3,
                open_positions: 1,
                manual_bags: 2,
                diverged: 1,
                pending_buys: 0,
                breaker_halted: false,
                day_net: 3.25,
                cash_usd: 90.5,
                equity_usd: 102.25,
                data_paused: false,
                rpc_endpoints: 2,
                rpc_healthy: 2,
                rpc_failover_reads: 3,
                rpc_inconsistent: false,
                oldest_open_age_secs: 4_400,
                stream_gaps_shed: 7,
                stream_pages_capped: 1,
                giveups_window: 1,
            },
            pnl: status::PnlReport {
                realized_pnl: -1.25,
                unrealized_pnl: 0.75,
                mark_value: 10.75,
                realizable_value: 10.5,
                overdraft_usd: 0.0,
                closed_count: 4,
                win_rate: Some(0.25),
            },
            positions: Vec::new(),
            manual_mints: Vec::new(),
            diverged_mints: Vec::new(),
            pending_buy_mints: Vec::new(),
            watchlist: status::WatchlistSnapshot {
                total: 0,
                entries: Vec::new(),
                min_buyers: 5,
                window_secs: 10,
            },
            heat_live_extra: 0,
            exposure_live_bags: 0,
            exposure_paper_locked: 0.0,
            day_tally_damaged: false,
        }
    }

    #[test]
    fn config_deny_unknown_fields_toml_parse() {
        assert_eq!(
            parse_config("guild_id = 123").expect("ok"),
            SidecarConfig {
                guild_id: 123,
                operator_ids: Vec::new(),
            }
        );
        // Typo trap: unknown keys refuse, never silently ignore.
        assert!(parse_config("guild_id = 123\nguildi_d = 456").is_err());
        assert!(parse_config("").is_err());
    }

    #[test]
    fn config_parses_operator_allowlist() {
        let cfg = parse_config("guild_id = 123\noperator_ids = [111, 222]").expect("ok");
        assert_eq!(cfg.operator_ids, vec![111, 222]);
        assert!(is_operator(&cfg.operator_ids, 111));
        assert!(is_operator(&cfg.operator_ids, 222));
        assert!(!is_operator(&cfg.operator_ids, 333));
        // Empty (default) = nobody: fail closed, never fail open.
        let bare = parse_config("guild_id = 123").expect("ok");
        assert!(bare.operator_ids.is_empty());
        assert!(!is_operator(&bare.operator_ids, 111));
    }

    #[test]
    fn status_compact_serves_readings_not_money() {
        let now = 1_000_000;
        let text = status_compact_text(&fixture_report(now - 3), now);
        assert!(text.contains("`paper·adaptive`"));
        assert!(text.contains("up 2h 5m"));
        assert!(text.contains("Breaker: `armed`"));
        assert!(text.contains("RPC: `2/2 ok · failovers 3`"));
        assert!(text.contains("Open 1 · bags 2 · diverged 1"));
        assert!(text.contains("Oldest open: 1h 13m"));
        assert!(text.contains("status age: 3s"));
        assert!(!text.contains("STALE"));
        assert!(!text.contains("90.5"), "compact status hides money figures");
        // The outage warning rides the render banner once (render_status
        // formats "bot may be down" ahead of this body); the body keeps
        // only the honest age line.
        let stale = status_compact_text(&fixture_report(now - 500), now);
        assert!(!stale.contains("STALE — bot may be down"));
        assert!(!stale.contains("bot may be down"));
        assert!(stale.contains("status age: 500s"));
        assert!(!stale.contains("102"), "no equity in the compact view");
    }

    #[test]
    fn no_status_text_fails_closed() {
        let text = no_status_text("No such file or directory");
        assert!(text.contains("No readable on-disk status"));
        assert!(text.contains("Nothing to report, no action taken."));
    }

    #[test]
    fn sell_gate_text_passes_fresh_with_daemon() {
        let now = 1_000_000;
        assert_eq!(sell_gate_text(now - 5, now, true), None);
        // The stale limit is inclusive: exactly STALE_WARN_SECS passes.
        assert_eq!(sell_gate_text(now - STALE_WARN_SECS, now, true), None);
    }

    #[test]
    fn sell_gate_text_refuses_stale_book_first() {
        let now = 1_000_000;
        let text = sell_gate_text(now - STALE_WARN_SECS - 1, now, true).expect("stale refusal");
        assert!(text.contains("Status is "), "reporter age code: {text}");
        assert!(text.contains("no action taken."));
        // Stale takes precedence over the daemon check: the message
        // must be the stale one even when the daemon is also gone.
        let both = sell_gate_text(now - STALE_WARN_SECS - 1, now, false).expect("stale refusal");
        assert!(both.contains("Status is "), "stale wins: {both}");
        assert!(!both.contains("daemon"));
    }

    #[test]
    fn sell_gate_text_refuses_daemon_down() {
        let now = 1_000_000;
        let text = sell_gate_text(now - 5, now, false).expect("daemon refusal");
        assert!(text.contains("No daemon running"));
        assert!(text.contains("No action taken."));
    }

    #[test]
    fn heartbeat_card_reuses_the_production_card() {
        let now = 1_000_000;
        let plain = Alerter::format_heartbeat(&fixture_report(now - 4).report);
        let card = heartbeat_card(&fixture_report(now - 4), now);
        // Same content the 6h alert card renders, plus freshness...
        assert!(card.content.starts_with(plain.content.as_str()));
        assert!(card.content.contains("(status age: 4s)"));
        // ...and the same embed (embed-only wire: never an Option).
        let embed = &card.embed;
        assert_eq!(embed.title, "Bot Heartbeat");
        assert_eq!(embed.fields.len(), 19);
    }

    #[test]
    fn heartbeat_notes_add_to_the_embed_never_mirror_it() {
        let now = 1_000_000;
        let report = fixture_report(now - 4);
        let card = heartbeat_card(&report, now);
        let notes = heartbeat_notes(&report, now);
        // Additive only: the freshness line rides the description.
        assert!(notes.contains("status age: 4s"));
        // The card's text view mirrors every embed field ("• **Mode:**");
        // the description must not repeat the same data.
        assert!(
            !notes.contains("• **"),
            "description must not mirror the structured fields"
        );
        assert!(card.content.contains("• **"), "the text view does mirror");
        // A damaged tally keeps its caveat in the additive notes.
        let damaged = StatusReport {
            day_tally_damaged: true,
            ..fixture_report(now - 4)
        };
        assert!(heartbeat_notes(&damaged, now).contains("tally damaged"));
    }

    #[test]
    fn embed_from_maps_alert_view() {
        let card = Alerter::format_entry(
            &alert::CardCtx {
                mode: "live",
                strategy: "scalp",
            },
            "MINT",
            0.000_004_2,
            40.0,
            -10.0,
            10.0,
            "model",
        );
        // CreateEmbed serializes to the exact wire shape — assert there.
        let shape = serde_json::to_value(embed_from(&card.embed)).expect("embed serializes");
        assert_eq!(shape["title"], "Trade Opened — MINT");
        assert_eq!(shape["color"], alert::discord::COLOR_GREEN);
        assert_eq!(shape["url"], "https://explorer.solana.com/address/MINT");
        assert_eq!(shape["fields"][0]["name"], "Mode");
        assert_eq!(shape["fields"][7]["name"], "Explorer");
        assert_eq!(shape["footer"]["text"], "live · scalp");
    }

    #[test]
    fn resolve_config_path_prefers_override() {
        let guard =
            crate::test_env::scoped(&[("DISCORD_CONFIG_FILE", Some("/tmp/sidecar-test.toml"))]);
        assert_eq!(
            resolve_config_path(),
            std::path::PathBuf::from("/tmp/sidecar-test.toml")
        );
        drop(guard);
    }

    /// Positions fixture: one priced, one unpriced, one stuck.
    fn positions_fixture(now: u64) -> StatusReport {
        let mut report = fixture_report(now - 3);
        report.report.open_positions = 2;
        report.pnl = status::PnlReport {
            realized_pnl: -1.25,
            unrealized_pnl: 1.68,
            mark_value: 12.43,
            realizable_value: 12.18,
            overdraft_usd: 0.0,
            closed_count: 4,
            win_rate: Some(0.25),
        };
        report.positions = vec![
            status::OpenPosition {
                id: 2,
                mint: "EjND2YSo7ySRbvYSFETYu7inQMSHzcFraWmxJTdcgYG5".to_string(),
                entry_price: 0.000_004_2,
                quantity: 2_400_000,
                entry_notional: 10.0,
                opened_at: now - 780,
                timeout_seconds: 3_600,
                mark_price: Some(0.000_004_9),
                unrealized_usd: Some(1.68),
            },
            status::OpenPosition {
                id: 3,
                mint: "GCDyYxgMC5oDbvYSFETYu7inQMSHzcFraWmxJTdcgYG6".to_string(),
                entry_price: 0.000_000_9,
                quantity: 8_888_888,
                entry_notional: 8.0,
                opened_at: now - 120,
                timeout_seconds: 0,
                mark_price: None,
                unrealized_usd: None,
            },
        ];
        report
    }

    fn watchlist_fixture(now: u64) -> StatusReport {
        let mut report = fixture_report(now);
        report.watchlist = status::WatchlistSnapshot {
            total: 2,
            entries: vec![
                status::WatchEntry {
                    mint: "FjND2YSo7ySRbvYSFETYu7inQMSHzcFraWmxJTdcgYG7".to_string(),
                    buyers: 3,
                    created_secs: now - 20,
                    deadline_secs: now + 17,
                    audit_score: Some(80),
                },
                status::WatchEntry {
                    mint: "HjND2YSo7ySRbvYSFETYu7inQMSHzcFraWmxJTdcgYG8".to_string(),
                    buyers: 9,
                    created_secs: now - 400,
                    deadline_secs: now - 12,
                    audit_score: None,
                },
            ],
            min_buyers: 5,
            window_secs: 10,
        };
        report
    }

    #[test]
    fn positions_text_renders_marks_and_time_parts() {
        let now = 1_000_000;
        let report = positions_fixture(now);
        let text = positions_text(&report, now);
        // Header carries the exit-discount (mark minus realizable).
        assert!(
            text.contains("2 open · mark $12.43 · exit-discount $0.25"),
            "{text}"
        );
        // Priced line: short mint, tokens, mark, signed pnl.
        assert!(text.contains("#2 EjND…gYG5"), "{text}");
        assert!(text.contains("2.40M tok @ 0.0000042 = $10.00"), "{text}");
        assert!(text.contains("mark 0.0000049 (+16.8% · +$1.68)"), "{text}");
        assert!(text.contains("held 13m · timeout in 47m"), "{text}");
        // Unpriced honest line.
        assert!(text.contains("#3 GCDy…gYG6"), "{text}");
        assert!(text.contains("no fresh mark — held at basis"), "{text}");
        assert!(text.contains("held 2m · no timeout"), "{text}");
        // The panel itself carries no freshness line — with_freshness
        // (render level) adds banner + age.
        assert!(!text.contains("status age"));
        assert!(!text.contains("STALE"));
    }

    #[test]
    fn positions_text_marks_stuck_and_lists_bags() {
        let now = 1_000_000;
        let mut report = positions_fixture(now);
        report.positions[0].timeout_seconds = 60;
        report.positions[0].opened_at = now - 120;
        report.manual_mints = vec!["FjND2YSo7ySRbvYSFETYu7inQMSHzcFraWmxJTdcgYG7".to_string()];
        report.pending_buy_mints = vec!["HjND2YSo7ySRbvYSFETYu7inQMSHzcFraWmxJTdcgYG8".to_string()];
        let text = positions_text(&report, now);
        assert!(text.contains("⚠ STUCK +1m past timeout"), "{text}");
        assert!(
            text.contains("Unpaired live bags (watch-only): FjND…gYG7"),
            "{text}"
        );
        assert!(
            text.contains("Pending buys (unconfirmed, watch-only): HjND…gYG8"),
            "{text}"
        );
    }

    #[test]
    fn positions_text_empty_book_still_discloses_live() {
        let now = 1_000_000;
        let mut report = fixture_report(now + 3);
        report.report.open_positions = 0;
        let text = positions_text(&report, now);
        assert!(text.contains("**Positions** — none open."), "{text}");
        assert!(!text.contains("mark"), "{text}");
    }

    #[test]
    fn pnl_text_discloses_the_panel() {
        let now = 1_000_000;
        let report = positions_fixture(now);
        let text = pnl_text(&report);
        assert!(
            text.contains("**PnL** — day +$3.25 · breaker `armed`"),
            "{text}"
        );
        assert!(
            text.contains("realized -$1.25 over 4 closed · win rate 25%"),
            "{text}"
        );
        assert!(
            text.contains("unrealized +$1.68 (realizable basis)"),
            "{text}"
        );
        assert!(
            text.contains("mark $12.43 → would net $12.18 closing full size (exit-discount $0.25)"),
            "{text}"
        );
        assert!(text.contains("Cash $90.50 · equity $102.25"), "{text}");
        assert!(!text.contains("STUCK"));
    }

    #[test]
    fn pnl_text_flags_overdraft() {
        let mut report = fixture_report(1_000_000);
        report.pnl.overdraft_usd = 4.5;
        report.report.cash_usd = -4.5;
        report.report.equity_usd = 95.5;
        let text = pnl_text(&report);
        assert!(text.contains("⚠ OVERDRAWN by $4.50"), "{text}");
        assert!(text.contains("Cash -$4.50"), "{text}");
    }

    #[test]
    fn with_freshness_banners_stale_file() {
        let now = 1_000_000;
        let fresh = fixture_report(now - 3);
        let text = with_freshness(String::from("panel"), &fresh, now);
        assert_eq!(text, "panel\nstatus age: 3s");
        let stale = fixture_report(now - 500);
        let text = with_freshness(String::from("panel"), &stale, now);
        assert!(
            text.contains(
                "⚠ Status is 500s old (limit 60s) — bot may be down.\n\npanel\nstatus age: 500s"
            ),
            "{text}"
        );
    }

    #[test]
    fn watchlist_text_reads_against_thresholds() {
        let now = 1_000_000;
        let text = watchlist_text(&watchlist_fixture(now), now);
        assert!(text.contains("**Watchlist** — 2 pending"), "{text}");
        assert!(text.contains("window 10s"), "{text}");
        assert!(
            text.contains("FjND…gYG7 — 3/5 buyers · expires in 17s"),
            "{text}"
        );
        assert!(
            text.contains("HjND…gYG8 — 9/5 buyers · ⚠ deadline passed (aborts at next poll)"),
            "{text}"
        );
    }

    #[test]
    fn watchlist_text_caps_honestly() {
        let now = 1_000_000;
        let mut report = watchlist_fixture(now);
        report.watchlist.total = 75;
        report.watchlist.entries.truncate(1);
        let text = watchlist_text(&report, now);
        assert!(
            text.contains("75 pending (window 10s, showing 1 of 75)"),
            "{text}"
        );
        assert!(!text.contains("2 pending"), "{text}");
    }

    #[test]
    fn helpers_keep_operator_units() {
        assert_eq!(dur(45), "45s");
        assert_eq!(dur(780), "13m");
        assert_eq!(dur(7_500), "2h 5m");
        assert_eq!(dur(97_200), "1d 3h");
        assert_eq!(tok_count(2_400_000), "2.40M");
        assert_eq!(tok_count(1_234), "1.2K");
        assert_eq!(tok_count(42), "42");
        assert_eq!(short_mint("short"), "short");
        assert_eq!(
            short_mint("EjND2YSo7ySRbvYSFETYu7inQMSHzcFraWmxJTdcgYG5"),
            "EjND…gYG5"
        );
        assert_eq!(signed_usd(-1.25), "-$1.25");
        assert_eq!(signed_pct(16.8), "+16.8%");
    }

    #[test]
    fn config_text_redacts_endpoints_surface_typos() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[network]\nhttp_url = \"https://api-key-leak.fqdn?v=K3Y\"\n\
             [trading]\nnotional_usd = 12.5\n\
             [risk]\nstrategy_mode = \"runner\"\n\
             [alerts]\ndiscord_alerts = true\ndiscord_channel = \"555\"\n",
        )
        .expect("write");
        let guard = crate::test_env::scoped(&[(
            crate::config::CONFIG_FILE_ENV,
            Some(path.to_string_lossy().as_ref()),
        )]);
        let text = config_text();
        drop(guard);
        assert!(text.contains("runner"), "{text}");
        assert!(text.contains("paper $12.50 per trade"), "{text}");
        assert!(text.contains("http set"), "{text}");
        assert!(text.contains("REDACTED"), "{text}");
        assert!(!text.contains("api-key-leak"), "{text}");
        assert!(!text.contains("K3Y"), "{text}");
        assert!(
            text.contains("Values are the file as read just now"),
            "{text}"
        );
    }

    #[test]
    fn config_text_refuses_broken_files() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[trading\nnotional_usd = 12.5\n").expect("write");
        let guard = crate::test_env::scoped(&[(
            crate::config::CONFIG_FILE_ENV,
            Some(path.to_string_lossy().as_ref()),
        )]);
        let text = config_text();
        drop(guard);
        assert!(text.starts_with("⚠ Config unreadable"), "{text}");
    }

    #[test]
    fn config_text_reports_unknown_keys_as_notes() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[trading]\nnotional_usd = 10\nnotioal_usd = 5\n").expect("write");
        let guard = crate::test_env::scoped(&[(
            crate::config::CONFIG_FILE_ENV,
            Some(path.to_string_lossy().as_ref()),
        )]);
        let text = config_text();
        drop(guard);
        assert!(text.contains("⚠ Config notes"), "{text}");
        assert!(text.contains("notioal_usd"), "{text}");
    }

    #[test]
    fn load_config_refuses_non_guild_ids() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("discord.toml");
        std::fs::write(&path, "guild_id = 0\n").expect("write");
        let guard = crate::test_env::scoped(&[(
            "DISCORD_CONFIG_FILE",
            Some(path.to_string_lossy().as_ref()),
        )]);
        let err = load_config().expect_err("zero id refused");
        assert!(err.contains("not a Discord id"), "{err}");
        assert!(err.contains("Copy Server ID"), "{err}");
        drop(guard);
    }

    #[test]
    fn load_config_reads_back_a_valid_guild() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("discord.toml");
        std::fs::write(&path, "guild_id = 987654321\n").expect("write");
        let guard = crate::test_env::scoped(&[(
            "DISCORD_CONFIG_FILE",
            Some(path.to_string_lossy().as_ref()),
        )]);
        let (_, cfg) = load_config().expect("valid config");
        assert_eq!(
            cfg,
            SidecarConfig {
                guild_id: 987_654_321,
                operator_ids: Vec::new(),
            }
        );
        drop(guard);
    }

    #[test]
    fn confirm_buttons_roundtrip_and_expire() {
        assert_eq!(
            parse_confirm_id(&confirm_custom_id("abc123", "confirm")),
            Some(("abc123", "confirm"))
        );
        assert_eq!(
            parse_confirm_id(&confirm_custom_id("abc123", "cancel")),
            Some(("abc123", "cancel"))
        );
        // Forged/foreign shapes never match: restart-invalidated or
        // mistyped buttons re-issue instead of executing.
        assert_eq!(parse_confirm_id("confirm"), None);
        assert_eq!(parse_confirm_id("ps2::confirm"), None);
        assert_eq!(parse_confirm_id("ps2:abc:launch"), None);
        assert_eq!(parse_confirm_id("other:abc:confirm"), None);
        // TTL boundary: clicks inside the window run, at/past it die.
        assert!(!confirm_expired(1_000, 1_000 + CONFIRM_TTL_SECS - 1));
        assert!(confirm_expired(1_000, 1_000 + CONFIRM_TTL_SECS));
        assert!(confirm_expired(1_000, 1_000 + CONFIRM_TTL_SECS + 60));
        // Clock skew (created in the future) never reads as expired.
        assert!(!confirm_expired(2_000, 1_000));
    }

    #[test]
    fn find_mint_classifies_every_list() {
        let now = 1_000_000;
        let mut report = fixture_report(now);
        report.positions = vec![status::OpenPosition {
            id: 7,
            mint: "OPENMINT11111111111111111111111111111111111".to_string(),
            entry_price: 0.001,
            quantity: 100,
            entry_notional: 10.0,
            opened_at: now,
            timeout_seconds: 0,
            mark_price: None,
            unrealized_usd: None,
        }];
        report.manual_mints = vec!["BAGMINT2222222222222222222222222222222222222".to_string()];
        report.diverged_mints = vec!["DIVERGE333333333333333333333333333333333333".to_string()];
        report.pending_buy_mints = vec!["PENDING4444444444444444444444444444444444444".to_string()];
        assert_eq!(
            find_mint(&report, "OPENMINT11111111111111111111111111111111111"),
            MintRef::Open(0)
        );
        assert_eq!(
            find_mint(&report, "BAGMINT2222222222222222222222222222222222222"),
            MintRef::ManualBag
        );
        assert_eq!(
            find_mint(&report, "DIVERGE333333333333333333333333333333333333"),
            MintRef::Diverged
        );
        assert_eq!(
            find_mint(&report, "PENDING4444444444444444444444444444444444444"),
            MintRef::PendingBuy
        );
        assert_eq!(
            find_mint(&report, "NOBODY5555555555555555555555555555555555555"),
            MintRef::Unknown
        );
    }

    #[test]
    fn audit_line_carries_attribution_never_secrets() {
        let event = AuditEvent {
            ts: 1_700_000_000,
            user_id: 111,
            user_name: "op",
            guild_id: 999,
            command: "sell",
            args: "MINT (requested by 111)",
            outcome: "executed",
        };
        let value = serde_json::to_value(&event).expect("audit serializes");
        assert_eq!(value["user_id"], 111);
        assert_eq!(value["command"], "sell");
        assert_eq!(value["outcome"], "executed");
        let raw = serde_json::to_string(&event).expect("line");
        assert!(!raw.contains("token"), "{raw}");
        assert!(!raw.contains("key"), "{raw}");
    }

    fn micro_row(ts: u64, net: f64, regime: Option<&str>) -> crate::replay::MicroRow {
        crate::replay::MicroRow {
            ts,
            net_pnl: net,
            micro: listener::MicroSignals {
                buys: 5,
                sells: 1,
                buy_ratio: Some(0.9),
                buy_volume_base: 5_000_000,
                sell_volume_base: 1_000_000,
                unique_traders: 4,
                top_wallet_share_pct: Some(60.0),
                creator_net_base: 0,
                velocity_delta: None,
            },
            regime: regime.map(str::to_string),
            model: Some("test-model".to_string()),
            source: "model".to_string(),
        }
    }

    #[test]
    fn research_text_headlines_paper_evidence() {
        let rows = vec![
            micro_row(1, -2.0, Some("momentum")),
            micro_row(2, 3.0, Some("momentum")),
        ];
        let s = crate::research::summarize(&rows).expect("universe nonempty");
        let backdrop = |ts: u64| crate::journal::JournalLine::Tick {
            ts,
            mint: "M".to_string(),
            mark: simulator::Mark::price_only(1.0),
            sol_feed: Some(100.0),
            tick_observed_at: ts,
            complete: false,
            manual_exits: false,
        };
        let tail = JournalTail {
            lines: vec![backdrop(1), backdrop(2)],
            scanned: 2,
            ..JournalTail::default()
        };
        let text = research_text(&s, &tail);
        assert!(
            text.contains("**Research** — paper patterns only"),
            "{text}"
        );
        assert!(
            text.contains("2 signaled exits · 1 losses · 1 wins"),
            "{text}"
        );
        assert!(text.contains("journal live read, 2 lines"), "{text}");
        assert!(text.contains("Top proposal:"), "{text}");
        assert!(text.contains("explicit approval"), "{text}");
        assert!(crate::research::summarize(&[]).is_none());
    }

    #[test]
    fn research_text_names_the_bounded_tail_and_bad_lines() {
        // L1/L2: a byte-capped scan must never read as a full replay,
        // and unreadable lines inside the scanned span are disclosed —
        // silence would fabricate a clean slate.
        let s = crate::research::summarize(&[micro_row(1, -2.0, Some("momentum"))])
            .expect("universe nonempty");
        let backdrop = |ts: u64| crate::journal::JournalLine::Tick {
            ts,
            mint: "M".to_string(),
            mark: simulator::Mark::price_only(1.0),
            sol_feed: Some(100.0),
            tick_observed_at: ts,
            complete: false,
            manual_exits: false,
        };
        let mut tail = JournalTail {
            lines: vec![backdrop(1)],
            scanned: 7,
            ..JournalTail::default()
        };
        let text = research_text(&s, &tail);
        assert!(
            text.contains("journal live read, last 1 of 7 lines"),
            "{text}"
        );
        tail.truncated = true;
        tail.bad = 3;
        let text = research_text(&s, &tail);
        assert!(text.contains("older history not scanned"), "{text}");
        assert!(text.contains("3 unreadable skipped"), "{text}");
        assert!(!text.contains("of 7"), "{text}");
    }

    #[test]
    fn risk_text_utilizes_caps_honestly() {
        // Default cap is 0 = OFF: the answer must not pretend an
        // armed day-loss breaker exists (L5).
        let s = Settings::from_file(None);
        let mut report = fixture_report(1_000_000);
        report.report.day_net = -4.0;
        report.report.open_positions = 1;
        let text = risk_text(&s, &report);
        assert!(text.contains("**Risk** — limit utilization"), "{text}");
        assert!(text.contains("cap off — no day-loss breaker"), "{text}");
        assert!(
            text.contains("cap off (day-loss breaker disabled)"),
            "{text}"
        );
        assert!(!text.contains("`armed`"), "{text}");
        assert!(text.contains("Breaker: "), "{text}");
        // Kill-switch state reads live, not from the file.
        assert!(text.contains("Entries: "), "{text}");
    }

    #[test]
    fn risk_text_matches_gate_utilization_for_heat_and_exposure() {
        // The collector publishes the gate's own inputs, so `/risk`
        // prints the same sums the gate compares — no re-derivation.
        let mut s = Settings::from_file(None);
        s.max_open_positions = 3;
        s.max_daily_loss_usd = 50.0;
        s.max_exposure_usd = 40.0;
        s.notional_usd = 10.0;
        let mut report = fixture_report(1_000_000);
        report.report.day_net = -12.5;
        report.report.open_positions = 1;
        report.heat_live_extra = 2;
        report.exposure_live_bags = 2;
        report.exposure_paper_locked = 10.0;
        let text = risk_text(&s, &report);
        // Heat: paper 1 + live-held 2 = 3/3, at the gate's refuse point
        // (`>= max` refuses, so the gate admits while used < max).
        assert!(text.contains("Opens 1 + 2 live-held = 3/3"), "{text}");
        assert!(text.contains("gate admits while used < 3"), "{text}");
        // Exposure: deployed $30, prospective $40 vs cap $40 (100%).
        assert!(
            text.contains(
                "exposure $30.00 + next $10.00 ticket = $40.00 vs cap $40.00 (100% committed)"
            ),
            "{text}"
        );
        // Day loss matches the breaker's own boundary: 12.5/50 = 25%.
        assert!(
            text.contains("day -$12.50 vs cap $50.00 (25% used)"),
            "{text}"
        );
        // Breaker armed, and armed honestly (cap on).
        assert!(text.contains("Breaker: `armed`"), "{text}");
    }

    #[test]
    fn risk_text_marks_damaged_tally_unknown_never_a_number() {
        // The collector publishes a finite placeholder with the damage
        // flag: renderers must print UNKNOWN, never the placeholder.
        let mut s = Settings::from_file(None);
        s.max_daily_loss_usd = 50.0;
        s.max_exposure_usd = 100.0;
        let mut report = fixture_report(1_000_000);
        report.report.breaker_halted = true;
        report.report.day_net = 0.0; // placeholder, not the tally
        report.day_tally_damaged = true;
        let text = risk_text(&s, &report);
        assert!(text.contains("day UNKNOWN (tally damaged"), "{text}");
        assert!(
            text.contains("HALTED — tally damaged (fail-closed), not the loss cap"),
            "{text}"
        );
        assert!(!text.contains("$0.00 vs cap"), "{text}");
        assert!(!text.contains("`armed`"), "{text}");
        // Undamaged over-cap halt: day and cap print once in the
        // utilization line; the breaker marker stays figure-free.
        report.day_tally_damaged = false;
        report.report.day_net = -51.0;
        let text = risk_text(&s, &report);
        assert!(text.contains("day -$51.00 vs cap $50.00"), "{text}");
        assert!(text.contains("Breaker: `HALTED`"), "{text}");
        assert_eq!(
            text.matches("day -$51.00").count(),
            1,
            "day figure prints once: {text}"
        );
        // Halted with the cap off and no damage: the utilization line
        // names the cap state; the breaker marker adds nothing twice.
        s.max_daily_loss_usd = 0.0;
        report.report.day_net = -4.0;
        let text = risk_text(&s, &report);
        assert!(text.contains("day -$4.00 vs cap off"), "{text}");
        assert!(text.contains("Breaker: `HALTED`"), "{text}");
    }

    #[test]
    fn pnl_text_marks_damaged_tally_unknown() {
        let mut report = fixture_report(1_000_000);
        report.report.breaker_halted = true;
        report.report.day_net = 0.0; // placeholder, not the tally
        report.day_tally_damaged = true;
        let text = pnl_text(&report);
        assert!(text.contains("day UNKNOWN (tally damaged)"), "{text}");
        assert!(text.contains("HALTED (tally damaged)"), "{text}");
        assert!(!text.contains("day +$0.00"), "{text}");
    }

    #[test]
    fn pnl_text_halt_never_repeats_the_day_figure() {
        let now = 1_000_000;
        let mut report = positions_fixture(now);
        report.report.breaker_halted = true;
        let text = pnl_text(&report);
        // Day net rides the day line exactly once; the breaker marker
        // is figure-free.
        assert!(text.contains("day +$3.25 · breaker `HALTED`"), "{text}");
        assert_eq!(
            text.matches("+$3.25").count(),
            1,
            "day figure prints once: {text}"
        );
    }

    #[test]
    fn heartbeat_card_caveats_damaged_tally() {
        let mut report = fixture_report(1_000_000 - 4);
        report.day_tally_damaged = true;
        report.report.day_net = 0.0;
        let card = heartbeat_card(&report, 1_000_000);
        assert!(
            card.content.contains("tally damaged — day figures unknown"),
            "{}",
            card.content
        );
        // Undamaged cards carry no caveat.
        let plain = heartbeat_card(&fixture_report(1_000_000 - 4), 1_000_000);
        assert!(
            !plain.content.contains("tally damaged"),
            "{}",
            plain.content
        );
    }

    #[test]
    fn status_compact_marks_damaged_breaker() {
        let mut report = fixture_report(1_000_000 - 3);
        report.report.breaker_halted = true;
        report.report.day_net = 0.0;
        report.day_tally_damaged = true;
        let text = status_compact_text(&report, 1_000_000);
        assert!(text.contains("HALTED (tally damaged)"), "{text}");
        // No money figures leak through the placeholder.
        assert!(!text.contains("+$0.00"), "{text}");
    }

    #[test]
    fn token_text_reports_book_watchlist_and_trail() {
        let now = 1_000_000;
        let report = watchlist_fixture(now);
        let mint = "FjND2YSo7ySRbvYSFETYu7inQMSHzcFraWmxJTdcgYG7";
        let open = crate::journal::JournalLine::Open {
            ts: now - 100,
            mint: mint.to_string(),
            risk: "model".to_string(),
            model: Some("m".to_string()),
            venue_fee_bps: 125.0,
            sol_feed: None,
            mark: simulator::Mark::price_only(1.0),
            ticket_usd: 10.0,
            position: simulator::Position {
                id: 1,
                mint: mint.to_string(),
                side: simulator::Side::Long,
                entry_price: 1.0,
                quantity: 10,
                entry_notional: 10.0,
                entry_fee: 0.0,
                tp_percent: 50.0,
                sl_percent: -10.0,
                trail_percent: 5.0,
                trailing_stop: false,
                best_price: 1.0,
                opened_at: now - 100,
                timeout_seconds: 3600,
                moonbag_percent: 0.0,
            },
            micro: None,
            regime: Some("momentum".to_string()),
            flip_loss_pct: None,
        };
        let trail = vec![&open];
        let tail = JournalTail {
            lines: vec![open.clone()],
            scanned: 1,
            ..JournalTail::default()
        };
        let text = token_text(&report, mint, &trail, &tail);
        assert!(text.contains("**Token** FjND…gYG7"), "{text}");
        assert!(text.contains("Watchlist: 3/5 buyers"), "{text}");
        assert!(text.contains("audit score 80"), "{text}");
        assert!(text.contains("open · risk model"), "{text}");
        // Watchlisted-and-unknown mint stays an honest absence, but the
        // book claim no longer covers "watched" (the sample semantics);
        // unknown + empty trail names the scanned span.
        let mut unknown = report.clone();
        unknown.report.open_positions = 0; // complete book sample
        let text = token_text(
            &unknown,
            "HjND2YSo7ySRbvYSFETYu7inQMSHzcFraWmxJTdcgYG8",
            &[],
            &tail,
        );
        assert!(text.contains("no record"), "{text}");
        assert!(
            text.contains("Trail: no journal entries for this mint in the last 1 lines"),
            "{text}"
        );
    }

    #[test]
    fn token_text_resolves_book_watchlist_contradiction_for_held_mints() {
        // L3: the gate pins a held mint's watch (curve stays live), so
        // a book line plus a raw quorum line would contradict itself
        // ("waiting for quorum" while already open).
        let now = 1_000_000;
        let mut report = watchlist_fixture(now);
        let mint = "FjND2YSo7ySRbvYSFETYu7inQMSHzcFraWmxJTdcgYG7";
        report.report.open_positions = 1;
        report.positions = vec![status::OpenPosition {
            id: 9,
            mint: mint.to_string(),
            entry_price: 0.001,
            quantity: 100,
            entry_notional: 10.0,
            opened_at: now - 60,
            timeout_seconds: 0,
            mark_price: None,
            unrealized_usd: None,
        }];
        let tail = JournalTail::default();
        let text = token_text(&report, mint, &[], &tail);
        assert!(text.contains("Book: open #9"), "{text}");
        assert!(
            text.contains("Watchlist: still tracked while held (3/5 buyers"),
            "{text}"
        );
        assert!(!text.contains("· audit score 80"), "{text}");
        assert!(!text.contains("Watchlist: 3/5 buyers"), "{text}");
    }

    #[test]
    fn token_text_honors_capped_samples_for_absence_claims() {
        // L3: open positions and watchlist entries both publish as
        // capped samples; an overflowed cap must downgrade every
        // absence claim ("not open" / "not watched") to "not in the
        // shown sample".
        let now = 1_000_000;
        let mut report = watchlist_fixture(now);
        report.report.open_positions = 61;
        report.positions = vec![status::OpenPosition {
            id: 1,
            mint: "EjND2YSo7ySRbvYSFETYu7inQMSHzcFraWmxJTdcgYG5".to_string(),
            entry_price: 0.001,
            quantity: 100,
            entry_notional: 10.0,
            opened_at: now - 60,
            timeout_seconds: 0,
            mark_price: None,
            unrealized_usd: None,
        }];
        report.watchlist.total = 75;
        let tail = JournalTail::default();
        let text = token_text(
            &report,
            "ZZZ1111111111111111111111111111111111111111",
            &[],
            &tail,
        );
        assert!(
            text.contains("not in the shown sample — 61 open, 1 shown"),
            "{text}"
        );
        assert!(
            text.contains("absence here is not proof of absence"),
            "{text}"
        );
        assert!(
            text.contains("not in the shown sample — 2 of 75 shown"),
            "{text}"
        );
        // Complete book + watching mints still claim exactly.
        report.report.open_positions = 0;
        report.positions.clear();
        report.watchlist.total = 2;
        let text = token_text(
            &report,
            "ZZZ1111111111111111111111111111111111111111",
            &[],
            &tail,
        );
        assert!(
            text.contains("Book: no record (not open, bagged, diverged, or pending)"),
            "{text}"
        );
        // Equal count = complete sample: plain absence, no hedge.
        assert!(!text.contains("not in the shown sample"), "{text}");
        assert!(!text.contains("not proof"), "{text}");
    }

    #[test]
    fn token_text_discloses_bad_journal_lines_behind_a_readable_file() {
        // L2: a corrupt-but-readable journal must not read as a clean
        // slate for an untraced mint.
        let now = 1_000_000;
        let report = fixture_report(now);
        let tail = JournalTail {
            bad: 4,
            scanned: 11,
            truncated: true,
            ..JournalTail::default()
        };
        let text = token_text(
            &report,
            "ZZZ1111111111111111111111111111111111111111",
            &[],
            &tail,
        );
        assert!(
            text.contains("no journal entries for this mint in the last 11 lines"),
            "{text}"
        );
        assert!(text.contains("4 unreadable skipped"), "{text}");
        assert!(text.contains("older history not scanned"), "{text}");
        // An actually-unreadable journal says so outright.
        let tail = JournalTail {
            error: Some("permission denied".to_string()),
            ..JournalTail::default()
        };
        let text = token_text(
            &report,
            "ZZZ1111111111111111111111111111111111111111",
            &[],
            &tail,
        );
        assert!(
            text.contains("journal unreadable — absence here proves nothing"),
            "{text}"
        );
    }

    // ---- Tier 2 audit-failure contracts + confirm-map bounds ----

    /// The embedded answer's description as text (embed-only wire:
    /// every reply is exactly one embed; this reads its body back the
    /// same way a client would render it).
    fn reply_description(message: &CreateInteractionResponseMessage) -> Option<String> {
        serde_json::to_value(message)
            .ok()?
            .get("embeds")?
            .get(0)?
            .get("description")?
            .as_str()
            .map(str::to_string)
    }

    fn sidecar() -> Sidecar {
        Sidecar {
            guild_id: GuildId::new(1),
            operators: vec![2],
            pending: Arc::new(Mutex::new(HashMap::new())),
            nonce: AtomicU64::new(0),
        }
    }

    /// Isolate audit/marker path resolution: `LOG_FILE` steers every
    /// state file used here into one temp dir.
    struct AuditScenario {
        _env: crate::test_env::Scoped,
        dir: tempfile::TempDir,
    }

    impl AuditScenario {
        fn new(_tag: &str) -> Self {
            let dir = tempfile::tempdir().expect("tmpdir");
            let env = crate::test_env::scoped(&[
                (
                    "LOG_FILE",
                    Some(dir.path().join("bot.log").to_str().expect("utf8")),
                ),
                (
                    "PID_FILE",
                    Some(dir.path().join("bot.pid").to_str().expect("utf8")),
                ),
            ]);
            Self { _env: env, dir }
        }

        /// A fresh status file plus a running-daemon pid entry (the
        /// test process itself is alive), so the shared `/sell` gate
        /// passes end-to-end. `manual_mint`, when given, lands in the
        /// book's bag list so `find_mint` knows it.
        fn seed_gate(&self, manual_mint: Option<&str>) {
            let mut report = fixture_report(now_secs());
            if let Some(m) = manual_mint {
                report.manual_mints = vec![m.to_string()];
            }
            status::write(&status::status_path(), &report).expect("status");
            let (pid_file, _) = crate::cli::resolve_paths();
            let info = crate::cli::PidInfo {
                pid: std::process::id(),
                live: true,
                started_at: now_secs(),
            };
            crate::cli::write_private_file(&pid_file, info.to_json().as_bytes()).expect("pid file");
        }

        fn audit_file(&self) -> std::path::PathBuf {
            self.dir.path().join("discord-audit.jsonl")
        }

        /// The audit file exists, appends are read-only (two audit
        /// attempts fail for the same reason: disk state, not parsing).
        fn block_audit(&self) {
            std::fs::write(self.audit_file(), b"seed\n").expect("seed");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(self.audit_file(), std::fs::Permissions::from_mode(0o444))
                    .expect("chmod 0444");
            }
        }
    }

    impl Drop for AuditScenario {
        fn drop(&mut self) {
            // A read-only seed file would break the tempdir cleanup.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if self.audit_file().exists() {
                    let _ = std::fs::set_permissions(
                        self.audit_file(),
                        std::fs::Permissions::from_mode(0o644),
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn resume_refuses_without_a_durable_audit_line() {
        // The unsafe direction fails closed on a broken audit trail:
        // the pause marker survives, nothing cleared.
        let sc = AuditScenario::new("resume-refuse");
        sc.block_audit();
        let sidecar = sidecar();
        let marker = crate::cli::entries_paused_path();
        crate::cli::set_entries_paused_at(&marker).expect("must be paused");
        let text = sidecar
            .execute_confirmed(2, "op", 1, 2, PendingAction::Resume)
            .await;
        assert!(text.contains("REFUSED"), "{text}");
        assert!(
            crate::cli::marker_present(&marker),
            "no audit line, no resume — marker stays"
        );
    }

    #[tokio::test]
    async fn resume_executes_and_audits_when_writeable() {
        let sc = AuditScenario::new("resume-exec");
        let sidecar = sidecar();
        let marker = crate::cli::entries_paused_path();
        crate::cli::set_entries_paused_at(&marker).expect("must be paused");
        let text = sidecar
            .execute_confirmed(2, "op", 1, 2, PendingAction::Resume)
            .await;
        assert!(text.contains("Entries resumed"), "{text}");
        assert!(!crate::cli::marker_present(&marker));
        let log = std::fs::read_to_string(sc.audit_file()).expect("audit");
        assert!(log.contains("\"executing\""), "{log}");
        assert!(log.contains("\"executed\""), "{log}");
    }

    #[tokio::test]
    async fn sell_still_executes_when_audit_unwritable() {
        // Availability direction: an exit request must not depend on a
        // writable audit file. The reply names the gap loudly.
        let sc = AuditScenario::new("sell-audit-fail");
        sc.block_audit();
        sc.seed_gate(Some("11111111111111111111111111111111"));
        let sidecar = sidecar();
        let text = sidecar
            .execute_confirmed(
                2,
                "op",
                1,
                2,
                PendingAction::Sell {
                    mint: "11111111111111111111111111111111".to_string(),
                },
            )
            .await;
        assert!(text.contains("Sell queued"), "{text}");
        assert!(text.contains("Audit append failed"), "{text}");
        // The request file exists (drained by the daemon).
        let (_, log_file) = crate::cli::resolve_paths();
        let found = std::fs::read_dir(log_file.parent().unwrap())
            .expect("dir")
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().starts_with("sell-"));
        assert!(found, "sell request file must be on disk");
    }

    #[tokio::test]
    async fn pause_still_executes_when_audit_unwritable() {
        // Availability direction: the pause marker lands; audit failure
        // only warns in the reply.
        let sc = AuditScenario::new("pause-audit-fail");
        sc.block_audit();
        let sidecar = sidecar();
        let text = sidecar.pause_handle(2, "op", 1);
        assert!(text.contains("Entries paused"), "{text}");
        assert!(text.contains("Audit append failed"), "{text}");
        assert!(crate::cli::marker_present(
            &crate::cli::entries_paused_path()
        ));
    }

    #[tokio::test]
    async fn audit_file_is_created_private_at_creation() {
        let sc = AuditScenario::new("audit-0600");
        std::fs::remove_file(sc.audit_file()).ok();
        let sidecar = sidecar();
        sidecar.pause_handle(2, "op", 1); // single audit write creates the file
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(sc.audit_file())
                .expect("audit file")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "created owner-only from the first byte"
            );
        }
    }

    #[test]
    fn confirm_cap_refuses_when_full() {
        let sidecar = sidecar();
        {
            let mut map = sidecar.pending.lock().expect("pending lock");
            for i in 0..PENDING_CONFIRM_CAP {
                map.insert(
                    format!("pending#{i}"),
                    PendingConfirm {
                        action: PendingAction::Resume,
                        requester: 2,
                        created: now_secs(),
                    },
                );
            }
        }
        let answer = sidecar.issue_confirm(PendingAction::Resume, "detail".to_string(), 2);
        // Embed-only replies: the refusal text rides the description.
        let description =
            reply_description(&answer.message).expect("service refusal carries an embed");
        assert!(description.contains("queue is full"), "{description}");
        assert_eq!(
            sidecar.pending.lock().expect("pending lock").len(),
            PENDING_CONFIRM_CAP,
            "refusal inserts nothing"
        );
    }

    #[test]
    fn expired_confirms_are_reaped_without_a_click() {
        let sidecar = sidecar();
        {
            let mut map = sidecar.pending.lock().expect("pending lock");
            for i in 0..PENDING_CONFIRM_CAP {
                map.insert(
                    format!("pending#{i}"),
                    PendingConfirm {
                        action: PendingAction::Resume,
                        requester: 2,
                        // One entry expired; the rest are fresh.
                        created: if i == 0 {
                            now_secs().saturating_sub(CONFIRM_TTL_SECS + 60)
                        } else {
                            now_secs()
                        },
                    },
                );
            }
        }
        let answer = sidecar.issue_confirm(PendingAction::Resume, "detail".to_string(), 2);
        // Only the expired entry fell out of the map before the insert.
        let description =
            reply_description(&answer.message).expect("successful arm carries an embed");
        assert!(!description.contains("queue is full"), "{description}");
        let map = sidecar.pending.lock().expect("pending lock");
        assert_eq!(map.len(), PENDING_CONFIRM_CAP, "one reaped, one inserted");
        assert!(
            map.values()
                .all(|p| !confirm_expired(p.created, now_secs())),
            "no expired entry survives an issue"
        );
    }
}

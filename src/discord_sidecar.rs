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
//!   mint, expired/missing confirm, or audit failure answers "no action
//!   taken" — never invented values or half-applied control.
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

/// Compact `/status` answer (pure). No money figures by design — it is
/// a public channel reply; `/heartbeat` carries the ledger view.
fn status_compact_text(report: &StatusReport, now: u64) -> String {
    let r = &report.report;
    let age = now.saturating_sub(report.ts);
    let uptime_h = r.uptime_secs / 3_600;
    let uptime_m = (r.uptime_secs % 3_600) / 60;
    let oldest_h = r.oldest_open_age_secs / 3_600;
    let oldest_m = (r.oldest_open_age_secs % 3_600) / 60;
    let stale = if age > STALE_WARN_SECS {
        " ⚠ STALE — bot may be down"
    } else {
        ""
    };
    let brk = Alerter::breaker_line(r.breaker_halted, r.day_net);
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
        "**Status** — `{m}·{s}` · up {h}h {mm}m{stale}\n\
             Breaker: {brk} · Data: {data} · RPC: `{rpc}`\n\
             Open {o} · bags {b} · diverged {d} · pending buys {p} · watchlist {w}\n\
             Oldest open: {oh}h {om}m\n\
             status age: {age}s",
        m = r.mode,
        s = r.strategy,
        h = uptime_h,
        mm = uptime_m,
        stale = stale,
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

/// Full `/heartbeat` answer: the exact production card (text fallback
/// + embed) with the trailing-file freshness note.
fn heartbeat_card(report: &StatusReport, now: u64) -> Card {
    let age = now.saturating_sub(report.ts);
    let mut card = Alerter::format_heartbeat(&report.report);
    card.content = truncate_content(&format!("{}\n(status age: {age}s)", card.content));
    card
}

/// Turn the alert crate's embed view into a serenity embed (1:1 by
/// intent: same title/palette/url/footer/fields).
fn embed_from(embed: &alert::discord::Embed) -> CreateEmbed {
    let mut out = CreateEmbed::new().title(&embed.title).colour(embed.color);
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
    format!(
        "**PnL** — day {} · breaker {}\n\
         Book lifetime: realized {} over {} closed · win rate {win}\n\
         Open book: unrealized {} (realizable basis) · mark {} → would net {} closing full size (exit-discount {})\n\
         {cash_line}",
        signed_usd(r.day_net),
        Alerter::breaker_line(r.breaker_halted, r.day_net),
        signed_usd(pnl.realized_pnl),
        pnl.closed_count,
        signed_usd(pnl.unrealized_pnl),
        usd(pnl.mark_value),
        usd(pnl.realizable_value),
        usd(exit_discount),
        cash_line = cash_line,
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
            return format!(
                "⚠ Config unreadable at `{}`: {e} — the running bot refuses to boot with a broken file; fix it and retry.",
                path.display()
            );
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
    let ok = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
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
/// a loud warn, and the return tells the caller whether it landed.
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
    audit_append(&AuditEvent {
        ts: now_secs(),
        user_id,
        user_name,
        guild_id,
        command,
        args,
        outcome,
    })
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
    cmd.data.options.iter().find_map(|o| match &o.value {
        CommandDataOptionValue::String(s) => {
            if o.name == "mint" {
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
            message: CreateInteractionResponseMessage::new()
                .ephemeral(true)
                .content(
                    "Tier 2 control is allowlisted: your Discord user ID is not in `operator_ids` (`discord.toml`). No action taken.",
                ),
        })
    }

    /// Fresh trailing file or refusal: control acts on the book, so a
    /// stale book is a hard stop (doc §6 fail closed). `/pause` skips
    /// this (safe direction, marker semantics need no book). Boxed
    /// error: the message payload is large (clippy `result_large_err`).
    fn fresh_report(&self) -> Result<StatusReport, Box<Answer>> {
        match status::read(&status::status_path()) {
            Ok(report) if now_secs().saturating_sub(report.ts) <= STALE_WARN_SECS => Ok(report),
            Ok(report) => Err(Box::new(Answer {
                message: CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .content(truncate_content(&format!(
                        "⚠ Status is {}s old (limit {STALE_WARN_SECS}s) — the bot may be down. Control refuses a stale book: no action taken.",
                        now_secs().saturating_sub(report.ts)
                    ))),
            })),
            Err(re) => Err(Box::new(self.no_data(&re))),
        }
    }

    /// Issue a confirm prompt: stores the pending action, returns the
    /// ephemeral message with Confirm/Cancel buttons.
    fn issue_confirm(&self, action: PendingAction, detail: String, user_id: u64) -> Answer {
        let nonce = format!(
            "{:x}{:x}",
            now_secs(),
            self.nonce.fetch_add(1, Ordering::Relaxed)
        );
        self.pending.lock().expect("pending lock").insert(
            nonce.clone(),
            PendingConfirm {
                action,
                requester: user_id,
                created: now_secs(),
            },
        );
        let _ = user_id;
        Answer {
            message: CreateInteractionResponseMessage::new()
                .ephemeral(true)
                .content(truncate_content(&format!(
                    "{detail}\nConfirm within {}m — no confirm = no-op.",
                    CONFIRM_TTL_SECS / 60
                )))
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
                    .content("`/sell` needs a mint: `/sell mint:<address>`. No action taken."),
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
                    .content(format!("`{mint}` is not a base58 pubkey. No action taken.")),
            };
        }
        let report = match self.fresh_report() {
            Ok(r) => r,
            Err(answer) => {
                audit(
                    user_id,
                    &user_name,
                    self.guild_id.get(),
                    SELL_COMMAND,
                    &mint,
                    "denied_stale_status",
                );
                return *answer;
            }
        };
        let (pid_file, _) = crate::cli::resolve_paths();
        if crate::cli::running_daemon(&pid_file).is_none() {
            audit(
                user_id,
                &user_name,
                self.guild_id.get(),
                SELL_COMMAND,
                &mint,
                "denied_no_daemon",
            );
            return Answer {
                message: CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .content("No daemon running — a sell request would sit unread. Start the bot first. No action taken."),
            };
        }
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
                    message: CreateInteractionResponseMessage::new()
                        .ephemeral(true)
                        .content(format!(
                            "`{}` is in no book list (open, bags, diverged, pending). Unknown mint: no action taken.",
                            short_mint(&mint)
                        )),
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
        let path = crate::cli::entries_paused_path();
        let already = crate::cli::marker_present(&path);
        match crate::cli::set_entries_paused_at(&path) {
            Ok(_) => {
                let outcome = if already {
                    "executed_already_paused"
                } else {
                    "executed"
                };
                let landed = audit(
                    user_id,
                    &user_name,
                    self.guild_id.get(),
                    PAUSE_COMMAND,
                    "",
                    outcome,
                );
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
                Answer {
                    message: CreateInteractionResponseMessage::new()
                        .ephemeral(true)
                        .content(text),
                }
            }
            Err(e) => {
                audit(
                    user_id,
                    &user_name,
                    self.guild_id.get(),
                    PAUSE_COMMAND,
                    "",
                    "error_marker_write",
                );
                Answer {
                    message: CreateInteractionResponseMessage::new()
                        .ephemeral(true)
                        .content(format!(
                            "Pause marker write failed ({e}). Entries NOT paused."
                        )),
                }
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
                    .content("Entries were not paused; nothing to resume."),
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
        self.pending
            .lock()
            .expect("pending lock")
            .retain(|_, p| !confirm_expired(p.created, now_secs()));
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
                .content(truncate_content(&outcome_text))
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
                match crate::cli::clear_entries_paused_at(&crate::cli::entries_paused_path()) {
                    Ok(true) => {
                        let args = format!("requested by {requester}");
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
                    .content(
                        "This sidecar answers only in its configured server. Nothing disclosed.",
                    ),
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
                message: CreateInteractionResponseMessage::new()
                    .content(truncate_content(&status_compact_text(&report, now))),
            },
            Ok(report) => Answer {
                message: CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .content(truncate_content(&format!(
                        "⚠ Status is {}s old (limit {STALE_WARN_SECS}s) — bot may be down. Last readings:\n\n{}",
                        now.saturating_sub(report.ts),
                        status_compact_text(&report, now)
                    ))),
            },
            Err(re) => Answer {
                message: CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .content(truncate_content(&no_status_text(&re.to_string()))),
            },
        }
    }

    /// `/heartbeat`: the full production card from on-disk state.
    fn render_heartbeat(&self, now: u64) -> Answer {
        let inner = status::read(&status::status_path()).map(|r| heartbeat_card(&r, now));
        match inner {
            Ok(card) => {
                let message = match &card.embed {
                    Some(embed) => CreateInteractionResponseMessage::new()
                        .content(&card.content)
                        .embeds(vec![embed_from(embed)]),
                    // Reachable only if the card someday overflows its
                    // own limits: the text fallback is the answer.
                    None => CreateInteractionResponseMessage::new().content(&card.content),
                };
                Answer { message }
            }
            Err(re) => Answer {
                message: CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .content(truncate_content(&no_status_text(&re.to_string()))),
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
                .content(truncate_content(&no_status_text(&re.to_string()))),
        }
    }

    /// `/positions`: open paper lines plus unpaired live custody.
    /// Ephemeral: entry sizes and PnL are ledger data.
    fn render_positions(&self, now: u64) -> Answer {
        match self.read_report() {
            Ok(report) => Answer {
                message: CreateInteractionResponseMessage::new()
                    .ephemeral(true)
                    .content(with_freshness(positions_text(&report, now), &report, now)),
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
                    .content(with_freshness(pnl_text(&report), &report, now)),
            },
            Err(re) => self.no_data(&re),
        }
    }

    /// `/watchlist`: public (mints are chain data, buyers anonymous).
    fn render_watchlist(&self, now: u64) -> Answer {
        match self.read_report() {
            Ok(report) => Answer {
                message: CreateInteractionResponseMessage::new()
                    .content(truncate_content(&watchlist_text(&report, now))),
            },
            Err(re) => self.no_data(&re),
        }
    }

    /// `/config`: the operator's own file, endpoints redacted.
    fn render_config(&self) -> Answer {
        Answer {
            message: CreateInteractionResponseMessage::new()
                .content(truncate_content(&config_text())),
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

/// Capture the token (sync, pre-thread), load the config, connect.
/// Boot errors are fail-stop: an operator wants the sidecar refusing
/// to half-run, not a phantom gateway connection.
pub fn run(token: String) -> Result<(), String> {
    let (config_path, cfg) = load_config()?;
    let guild_id = GuildId::new(cfg.guild_id);
    let sidecar = Sidecar {
        guild_id,
        operators: cfg.operator_ids.clone(),
        pending: Arc::new(Mutex::new(HashMap::new())),
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
        let stale = status_compact_text(&fixture_report(now - 500), now);
        assert!(stale.contains("STALE — bot may be down"));
        assert!(!stale.contains("102"), "no equity in the compact view");
    }

    #[test]
    fn no_status_text_fails_closed() {
        let text = no_status_text("No such file or directory");
        assert!(text.contains("No readable on-disk status"));
        assert!(text.contains("Nothing to report, no action taken."));
    }

    #[test]
    fn heartbeat_card_reuses_the_production_card() {
        let now = 1_000_000;
        let plain = Alerter::format_heartbeat(&fixture_report(now - 4).report);
        let card = heartbeat_card(&fixture_report(now - 4), now);
        // Same content the 6h alert card renders, plus freshness...
        assert!(card.content.starts_with(plain.content.as_str()));
        assert!(card.content.contains("(status age: 4s)"));
        // ...and the same embed.
        assert!(card.embed.is_some());
        let embed = card.embed.expect("embed");
        assert_eq!(embed.title, "Bot Heartbeat");
        assert_eq!(embed.fields.len(), 19);
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
        let embed = card.embed.expect("entry embed");
        // CreateEmbed serializes to the exact wire shape — assert there.
        let shape = serde_json::to_value(embed_from(&embed)).expect("embed serializes");
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
                },
                status::WatchEntry {
                    mint: "HjND2YSo7ySRbvYSFETYu7inQMSHzcFraWmxJTdcgYG8".to_string(),
                    buyers: 9,
                    created_secs: now - 400,
                    deadline_secs: now - 12,
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
}

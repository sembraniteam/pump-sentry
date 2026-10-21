//! Direct Discord delivery (bot token HTTPS) plus card formatters.
//!
//! The Hermes subprocess dispatch is gone: [`Alerter::send`] POSTs the
//! card to `POST /channels/{id}/messages` with `Authorization: Bot …`.
//! The token is owned data captured at boot (env-only, scrubbed after
//! capture like the other bearer secrets) and never logged.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tracing::warn;

/// Env var (or `{var}_FILE`) carrying the Discord bot token. Secret:
/// env-only, never the config file, scrubbed after boot capture.
pub const DISCORD_BOT_TOKEN_ENV: &str = "DISCORD_BOT_TOKEN";
/// Discord API root for channel sends.
pub const DISCORD_API: &str = "https://discord.com/api/v10";
/// Per-message dispatch deadline.
pub const DISPATCH_TIMEOUT: Duration = Duration::from_secs(5);
/// Max in-flight sends. Excess drops with a counter.
pub const MAX_INFLIGHT_ALERTS: usize = 4;
/// Discord `content` length cap: longer cards are truncated, never split.
pub const MAX_CONTENT_LEN: usize = 2000;

/// Explorer deep-link roots (single source; cards reference these).
pub const EXPLORER_ADDRESS: &str = "https://explorer.solana.com/address";
pub const EXPLORER_TX: &str = "https://explorer.solana.com/tx";

/// Fire-and-forget alerter. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Alerter {
    /// Bot token (`Authorization: Bot …`). `None` disables sends.
    token: Option<String>,
    api_base: String,
    timeout: Duration,
    /// Channel id target. `None` disables sends.
    /// Non-numeric ids are rejected at the builder.
    channel: Option<String>,
    /// Permits shared across clones.
    permits: Arc<Semaphore>,
    /// Messages dropped for lack of a permit.
    dropped: Arc<std::sync::atomic::AtomicU64>,
}

/// Mode + strategy tag carried by every card. Both are
/// operator-facing config, never secrets.
#[derive(Debug, Clone, Copy)]
pub struct CardCtx<'a> {
    /// `live` or `paper`.
    pub mode: &'a str,
    /// `adaptive` | `scalp` | `runner`.
    pub strategy: &'a str,
}

/// Heartbeat payload: counters plus cash.
pub struct HeartbeatReport<'a> {
    /// `live` or `paper`.
    pub mode: &'a str,
    /// Effective strategy (`adaptive` | `scalp` | `runner`).
    pub strategy: &'a str,
    /// Seconds since boot.
    pub uptime_secs: u64,
    /// Listener throughput counters.
    pub logs_seen: u64,
    /// Launch candidates seen.
    pub candidates: u64,
    /// Candidates parsed.
    pub parse_ok: u64,
    /// Dropped events (full + throttled).
    pub dropped: u64,
    /// Watchlisted mints awaiting quorum.
    pub watchlist_pending: usize,
    /// Open paper positions.
    pub open_positions: usize,
    /// Live bags held for manual exit. Hidden from paper equity.
    pub manual_bags: usize,
    /// Paper-open but live-missing (no fill): needs review/funding.
    pub diverged: usize,
    /// Buys awaiting confirmation (unknown fills).
    pub pending_buys: usize,
    /// Breaker currently halting entries.
    pub breaker_halted: bool,
    /// Day net PnL (USD) behind the breaker line.
    pub day_net: f64,
    /// Free paper cash (USD).
    pub cash_usd: f64,
    /// Mark-to-market paper equity (USD).
    pub equity_usd: f64,
    /// Data-degrade pause active (RPC pool / SOL feed / listener).
    pub data_paused: bool,
    /// RPC read pool: configured endpoints.
    pub rpc_endpoints: usize,
    /// RPC read pool: currently eligible endpoints.
    pub rpc_healthy: usize,
    /// RPC read pool: reads served by a failover endpoint.
    pub rpc_failover_reads: u64,
    /// RPC read pool: endpoints disagree on slot (views inconsistent).
    pub rpc_inconsistent: bool,
    /// Age (secs) of the oldest open paper position (0 = none).
    pub oldest_open_age_secs: u64,
    /// Backfill-capped reconnect gaps shed (slots never replayed).
    pub stream_gaps_shed: u64,
    /// WS reconnect backfills capped with unseen history beyond
    /// (launches lost, count unknown).
    pub stream_pages_capped: u64,
    /// Dispatch GiveUps (buy/sell failures) in the trailing window.
    pub giveups_window: u64,
}

impl Default for Alerter {
    fn default() -> Self {
        Self::new()
    }
}

/// Capture the bot token from the environment (or `{var}_FILE`, the
/// 0600-file form that avoids exporting the secret). Pure helper for
/// the sync boot phase: capture, then scrub the env.
pub fn capture_bot_token() -> Option<String> {
    std::env::var(DISCORD_BOT_TOKEN_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var(format!("{DISCORD_BOT_TOKEN_ENV}_FILE"))
                .ok()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Channel-send URL for a numeric channel id (pure).
fn send_url(api_base: &str, channel: &str) -> String {
    format!(
        "{}/channels/{channel}/messages",
        api_base.trim_end_matches('/')
    )
}

/// Truncate a card to Discord's `content` cap (pure). Truncation keeps
/// char boundaries and marks the cut; formatters sanitize fields
/// already, so the marker text is safe to append.
fn truncate_content(message: &str) -> String {
    if message.len() <= MAX_CONTENT_LEN {
        return message.to_string();
    }
    let mut end = MAX_CONTENT_LEN;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &message[..end])
}

impl Alerter {
    /// Alerter shell: sends only once a token and a channel are set
    /// (both builders below). Until then [`Alerter::send`] no-ops.
    pub fn new() -> Self {
        Self::with_api_base(DISCORD_API)
    }

    /// Same shell against another API root (tests point at loopback).
    fn with_api_base(api_base: &str) -> Self {
        Self {
            token: None,
            api_base: api_base.to_string(),
            timeout: DISPATCH_TIMEOUT,
            channel: None,
            permits: Arc::new(Semaphore::new(MAX_INFLIGHT_ALERTS)),
            dropped: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// No-op alerter: [`Alerter::send`] returns immediately.
    pub fn disabled() -> Self {
        Self::new()
    }

    /// Messages dropped for lack of a permit. Test-only: production
    /// surfaces drops via tracing, never by polling this counter.
    #[cfg(test)]
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Set the bot token (never logged). Empty input is ignored.
    pub fn with_token(mut self, token: &str) -> Self {
        let token = token.trim();
        if !token.is_empty() {
            self.token = Some(token.to_string());
        }
        self
    }

    /// Route to a Discord channel id. Bad input keeps the unset state
    /// (sends stay disabled).
    pub fn with_channel(mut self, channel: &str) -> Self {
        let channel = channel.trim();
        if !channel.is_empty() && channel.len() <= 32 && channel.bytes().all(|b| b.is_ascii_digit())
        {
            self.channel = Some(channel.to_string());
        }
        self
    }

    /// Send `message` as a detached task. Never blocks the caller.
    /// No token or no channel means no-op (boot logs the reason).
    /// Excess load drops with a counter.
    pub fn send(&self, message: String) {
        let (Some(token), Some(channel)) = (self.token.clone(), self.channel.clone()) else {
            return;
        };
        let permit = match self.permits.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                let dropped = self
                    .dropped
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    + 1;
                warn!(target: "alerting", dropped, "alert storm; dropping message");
                return;
            }
        };
        let url = send_url(&self.api_base, &channel);
        let timeout = self.timeout;
        let body = truncate_content(&message);
        tokio::spawn(async move {
            let _permit = permit;
            let client = reqwest::Client::new();
            let run = client
                .post(&url)
                .header("Authorization", format!("Bot {token}"))
                .json(&serde_json::json!({ "content": body }))
                .timeout(timeout)
                .send();
            match tokio::time::timeout(timeout, run).await {
                Err(_) => warn!(target: "alerting", "Discord alert timed out"),
                Ok(Err(e)) => warn!(target: "alerting", "Discord delivery failed: {e}"),
                Ok(Ok(resp)) if !resp.status().is_success() => {
                    warn!(target: "alerting", status = %resp.status(), "Discord delivery rejected");
                }
                Ok(Ok(_)) => {}
            }
        });
    }

    /// `live · adaptive`-style mode line shared by every card.
    fn mode_line(ctx: &CardCtx<'_>) -> String {
        let mode = Self::sanitize(ctx.mode);
        let strategy = Self::sanitize(ctx.strategy);
        format!("• **Mode:** `{mode} · {strategy}`")
    }

    /// Entry card. `risk` is the profile provenance (`model` |
    /// `heuristic`) so operators can tell model entries from the
    /// lower-bound fallback at a glance.
    pub fn format_entry(
        ctx: &CardCtx<'_>,
        mint: &str,
        entry_usd: f64,
        tp_pct: f64,
        sl_pct: f64,
        notional_usd: f64,
        risk: &str,
    ) -> String {
        let mint = Self::sanitize(mint);
        let risk = Self::sanitize(risk);
        format!(
            "🟢 **[TRADE OPENED]**\n\
             {}\n\
             • **Token Mint:** `{mint}`\n\
             • **Entry Price:** `${entry_usd:.8}`\n\
             • **Size:** `${notional_usd:.2}`\n\
             • **Target TP:** `+{tp_pct:.1}%` | **Stop Loss:** `{sl_pct:.1}%`\n\
             • **Risk:** `{risk}`\n\
             • **Explorer:** {EXPLORER_ADDRESS}/{mint}",
            Self::mode_line(ctx),
        )
    }

    /// Day-loss halt card. Fires once per UTC day (see
    /// `DailyBreaker::halt_state`); the heartbeat carries the state
    /// after that. Exits always work — only entries halt.
    pub fn format_halt(ctx: &CardCtx<'_>, cap_usd: f64, day_net: f64) -> String {
        format!(
            "🛑 **[DAY-LOSS HALT]**\n\
             {}\n\
             • **Cap:** `-${cap_usd:.2}/day` | **Day net:** `${day_net:+.2}`\n\
             • Entries halted; exits always work",
            Self::mode_line(ctx),
        )
    }

    /// Strip markdown and control chars from fields.
    /// Brackets and parens go too (no `[text](url)` links).
    fn sanitize(s: &str) -> String {
        s.chars()
            .filter(|c| {
                !matches!(
                    c,
                    '`' | '*'
                        | '_'
                        | '~'
                        | '|'
                        | '>'
                        | '@'
                        | '#'
                        | ':'
                        | '/'
                        | '\\'
                        | '['
                        | ']'
                        | '('
                        | ')'
                ) && !c.is_control()
                    && !is_format_char(*c)
            })
            .take(128)
            .collect()
    }

    /// Exit card.
    pub fn format_exit(
        ctx: &CardCtx<'_>,
        mint: &str,
        reason: &str,
        net_pnl: f64,
        pct: f64,
        equity: f64,
        is_moonbag: bool,
    ) -> String {
        let mint = Self::sanitize(mint);
        let reason = Self::sanitize(reason);
        let icon = if net_pnl >= 0.0 { "🎯" } else { "🛑" };
        let tag = if is_moonbag {
            "MOONBAG PARTIAL EXIT"
        } else {
            "POSITION CLOSED"
        };
        format!(
            "{icon} **[{tag}]**\n\
             {}\n\
             • **Token Mint:** `{mint}`\n\
             • **Exit Reason:** `{reason}`\n\
             • **Realized PnL:** `${net_pnl:+.2}` (`{pct:+.2}%`)\n\
             • **Portfolio Equity:** `${equity:.2}`",
            Self::mode_line(ctx),
        )
    }

    /// Live buy card (bundle id for tracking).
    pub fn format_live_buy(
        ctx: &CardCtx<'_>,
        mint: &str,
        bundle_id: &str,
        amount_tokens: u64,
    ) -> String {
        let mint = Self::sanitize(mint);
        let bundle_id = Self::sanitize(bundle_id);
        format!(
            "🚀 **[LIVE BUY SENT]**\n\
             {}\n\
             • **Token Mint:** `{mint}`\n\
             • **Amount:** `{amount_tokens}` base units\n\
             • **Bundle:** `{bundle_id}`\n\
             • **Explorer:** {EXPLORER_ADDRESS}/{mint}",
            Self::mode_line(ctx),
        )
    }

    /// Live sell dispatch card.
    pub fn format_live_sell(ctx: &CardCtx<'_>, mint: &str, bundle_id: &str) -> String {
        let mint = Self::sanitize(mint);
        let bundle_id = Self::sanitize(bundle_id);
        format!(
            "🔻 **[LIVE SELL SENT]**\n\
             {}\n\
             • **Token Mint:** `{mint}`\n\
             • **Bundle:** `{bundle_id}`\n\
             • **Explorer:** {EXPLORER_ADDRESS}/{mint}",
            Self::mode_line(ctx),
        )
    }

    /// Migration card. Missing ids render as `unknown`.
    pub fn format_migration(
        ctx: &CardCtx<'_>,
        mint: &str,
        destination: &str,
        signature: &str,
        amm_id: Option<&str>,
        base_mint: Option<&str>,
        quote_mint: Option<&str>,
    ) -> String {
        let mint = Self::sanitize(mint);
        let destination = Self::sanitize(destination);
        let signature = Self::sanitize(signature);
        let amm = amm_id
            .map(Self::sanitize)
            .unwrap_or_else(|| "unknown".to_string());
        let base = base_mint
            .map(Self::sanitize)
            .unwrap_or_else(|| "unknown".to_string());
        let quote = quote_mint
            .map(Self::sanitize)
            .unwrap_or_else(|| "unknown".to_string());
        format!(
            "🌉 **[MIGRATION DETECTED]**\n\
             {}\n\
             • **Token Mint:** `{mint}`\n\
             • **Destination:** `{destination}`\n\
             • **AMM:** `{}`\n\
             • **Base Mint:** `{}`\n\
             • **Quote Mint:** `{}`\n\
             • **Tx:** {EXPLORER_TX}/{signature}",
            Self::mode_line(ctx),
            amm,
            base,
            quote,
        )
    }

    /// Heartbeat card. Only mode/strategy need sanitize (rest are numbers).
    pub fn format_heartbeat(r: &HeartbeatReport<'_>) -> String {
        let mode = Self::sanitize(r.mode);
        let strategy = Self::sanitize(r.strategy);
        let uptime_h = r.uptime_secs / 3_600;
        let uptime_m = (r.uptime_secs % 3_600) / 60;
        let oldest_h = r.oldest_open_age_secs / 3_600;
        let oldest_m = (r.oldest_open_age_secs % 3_600) / 60;
        format!(
            "💚 **[BOT HEARTBEAT]**\n\
             • **Mode:** `{mode} · {strategy}`\n\
             • **Uptime:** `{uptime_h}h {uptime_m}m`\n\
             • **Logs seen:** `{}`\n\
             • **Candidates:** `{}`\n\
             • **Parsed:** `{}`\n\
             • **Dropped:** `{}`\n\
             • **Watchlist:** `{}` pending\n\
             • **Open:** `{}` position(s)\n\
             • **Oldest open:** `{oldest_h}h {oldest_m}m`\n\
             • **Manual bags:** `{}` held\n\
             • **Diverged:** `{}` (paper without live cover)\n\
             • **Pending buys:** `{}` unconfirmed\n\
             • **Breaker:** {}\n\
             • **Data:** {}\n\
             • **RPC:** `{}/{} ok · failovers {}{}`\n\
             • **Stream gaps shed:** `{}` slots (`{}` capped backfills)\n\
             • **GiveUps:** `{}` in window\n\
             • **Cash:** `${:.2}`\n\
             • **Equity:** `${:.2}`",
            r.logs_seen,
            r.candidates,
            r.parse_ok,
            r.dropped,
            r.watchlist_pending,
            r.open_positions,
            r.manual_bags,
            r.diverged,
            r.pending_buys,
            Self::breaker_line(r.breaker_halted, r.day_net),
            if r.data_paused {
                "`DEGRADED — entries paused`"
            } else {
                "`ok`"
            },
            r.rpc_healthy,
            r.rpc_endpoints,
            r.rpc_failover_reads,
            if r.rpc_inconsistent {
                " · INCONSISTENT"
            } else {
                ""
            },
            r.stream_gaps_shed,
            r.stream_pages_capped,
            r.giveups_window,
            r.cash_usd,
            r.equity_usd,
        )
    }

    /// Breaker line: `armed`, or `HALTED` with the day net that
    /// tripped it. Plain values, no secrets.
    fn breaker_line(halted: bool, day_net: f64) -> String {
        if halted {
            format!("`HALTED (day ${day_net:+.2})`")
        } else {
            "`armed`".to_string()
        }
    }

    /// Post-migration spike card.
    pub fn format_follow_spike(
        ctx: &CardCtx<'_>,
        mint: &str,
        multiple: f64,
        price_usd: f64,
    ) -> String {
        let mint = Self::sanitize(mint);
        format!(
            "🚀 **[POST-MIGRATION SPIKE]**\n\
             {}\n\
             • **Token Mint:** `{mint}`\n\
             • **Move:** `{multiple:.1}x off migration`\n\
             • **Price:** `${price_usd:.8}`\n\
             • **Explorer:** {EXPLORER_ADDRESS}/{mint}",
            Self::mode_line(ctx),
        )
    }
}

/// Format chars that `is_control` misses: zero-width and bidi marks.
fn is_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAPER: CardCtx = CardCtx {
        mode: "paper",
        strategy: "adaptive",
    };
    const LIVE: CardCtx = CardCtx {
        mode: "live",
        strategy: "scalp",
    };

    /// Base heartbeat fixture; tests override individual fields.
    #[cfg(test)]
    fn construct_heartbeat_fixture() -> HeartbeatReport<'static> {
        HeartbeatReport {
            mode: "live",
            strategy: "runner",
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
            stream_pages_capped: 0,
            giveups_window: 1,
        }
    }

    #[test]
    fn entry_card_marks_fields() {
        let card = Alerter::format_entry(&PAPER, "MINT", 0.00000420, 40.0, -10.0, 10.0, "model");
        assert!(card.contains("[TRADE OPENED]"));
        assert!(card.contains("`MINT`"));
        assert!(card.contains("$0.00000420"));
        assert!(card.contains("+40.0%"));
        assert!(card.contains("-10.0%"));
        assert!(card.contains("explorer.solana.com/address/MINT"));
        assert!(card.contains("`paper · adaptive`"));
    }

    #[test]
    fn exit_card_signs_and_tags() {
        let win = Alerter::format_exit(&PAPER, "M", "TakeProfit", 4.2, 42.0, 104.2, false);
        assert!(win.contains("🎯"));
        assert!(win.contains("POSITION CLOSED"));
        assert!(win.contains("$+4.20"));
        let loss = Alerter::format_exit(&PAPER, "M", "StopLoss", -1.5, -15.0, 98.5, false);
        assert!(loss.contains("🛑"));
        let moon = Alerter::format_exit(&PAPER, "M", "Manual", 0.0, 0.0, 100.0, true);
        assert!(moon.contains("MOONBAG PARTIAL EXIT"));
    }

    #[test]
    fn follow_spike_card_marks_multiple() {
        let card = Alerter::format_follow_spike(&PAPER, "M", 2.0, 0.01);
        assert!(card.contains("[POST-MIGRATION SPIKE]"));
        assert!(card.contains("2.0x"));
        assert!(card.contains("explorer.solana.com/address/M"));
    }

    #[test]
    fn heartbeat_card_marks_counters() {
        let card = Alerter::format_heartbeat(&HeartbeatReport {
            mode: "live",
            strategy: "runner",
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
        });
        assert!(card.contains("[BOT HEARTBEAT]"));
        assert!(card.contains("`live · runner`"));
        assert!(card.contains("2h 5m"));
        assert!(card.contains("`1000`"));
        assert!(card.contains("`2` held"));
        assert!(card.contains("$90.50"));
        assert!(card.contains("$102.25"));
        // Observability lines: health, RPC pool, stream gaps, failures.
        assert!(card.contains("**Data:** `ok`"));
        assert!(card.contains("**RPC:** `2/2 ok · failovers 3`"));
        assert!(card.contains("**Oldest open:** `1h 13m`"));
        assert!(card.contains("**Stream gaps shed:** `7` slots (`1` capped backfills)"));
        assert!(card.contains("**GiveUps:** `1` in window"));
        // Degraded state reads as paused, inconsistent as a flag.
        let degraded = Alerter::format_heartbeat(&HeartbeatReport {
            data_paused: true,
            rpc_healthy: 0,
            rpc_inconsistent: true,
            ..construct_heartbeat_fixture()
        });
        assert!(degraded.contains("**Data:** `DEGRADED — entries paused`"));
        assert!(degraded.contains("`0/2 ok · failovers 3 · INCONSISTENT`"));
    }

    #[test]
    fn migration_card_marks_pool() {
        let card = Alerter::format_migration(
            &LIVE,
            "M",
            "RaydiumV4",
            "sig",
            Some("AMM"),
            Some("BASE"),
            None,
        );
        assert!(card.contains("[MIGRATION DETECTED]"));
        assert!(card.contains("RaydiumV4"));
        assert!(card.contains("`AMM`"));
        assert!(card.contains("explorer.solana.com/tx/sig"));
        let bare = Alerter::format_migration(&LIVE, "M", "PumpSwap", "sig", None, None, None);
        assert!(bare.contains("unknown"));
    }

    #[test]
    fn interpolated_fields_are_sanitized() {
        // Attacker-shaped inputs must not survive as mentions, fences, or links.
        let card =
            Alerter::format_exit(&PAPER, "@everyone `x`", "TakeProfit", 1.0, 1.0, 1.0, false);
        assert!(!card.contains("@everyone"));
        assert!(!card.contains("`x`"));
        let mig = Alerter::format_migration(
            &PAPER,
            "M",
            "X",
            "https://evil.example/@everyone",
            Some("`AMM`"),
            None,
            None,
        );
        assert!(!mig.contains('@'));
        assert!(!mig.contains("https://evil.example"));
        // Backtick payloads collapse to plain text inside the template's own span.
        let tricky = Alerter::format_migration(&PAPER, "M", "X", "s", Some("`A`M`M`"), None, None);
        assert!(tricky.contains("`AMM`"));
    }

    #[test]
    fn sanitize_blocks_links_and_bidi() {
        let card = Alerter::format_exit(
            &PAPER,
            "[click](https://evil.example)",
            "TakeProfit",
            1.0,
            1.0,
            1.0,
            false,
        );
        assert!(!card.contains("https://evil.example"));
        assert!(!card.contains("[click]"));
        // Zero-width and bidi-override characters are stripped.
        let sneaky = "AB\u{200B}CD\u{202E}EF";
        assert_eq!(Alerter::sanitize(sneaky), "ABCDEF");
        assert_eq!(Alerter::sanitize("a\u{FEFF}b"), "ab");
    }

    #[test]
    fn storm_drops_behind_permits() {
        // Exhaust the permit pool; the next send drops instead of posting.
        let alert = Alerter::new()
            .with_token("test-token-fixture")
            .with_channel("123456789");
        let mut held = Vec::new();
        for _ in 0..MAX_INFLIGHT_ALERTS {
            held.push(alert.permits.clone().try_acquire_owned().expect("permit"));
        }
        alert.send("storm".to_string());
        assert_eq!(alert.dropped_count(), 1);
        drop(held);
    }

    #[test]
    fn channel_rejects_bad_input() {
        // Overlong, blank, or non-numeric input keeps sends disabled
        // (no channel set).
        assert!(
            Alerter::new()
                .with_channel(&"1".repeat(64))
                .channel
                .is_none()
        );
        assert!(Alerter::new().with_channel("  ").channel.is_none());
        assert!(
            Alerter::new()
                .with_channel("trades;rm -rf")
                .channel
                .is_none()
        );
        assert_eq!(
            Alerter::new().with_channel("123456789").channel.as_deref(),
            Some("123456789")
        );
    }

    #[test]
    fn send_url_shape() {
        assert_eq!(
            send_url("https://discord.com/api/v10", "123456789"),
            "https://discord.com/api/v10/channels/123456789/messages"
        );
    }

    #[test]
    fn truncate_content_marks_the_cut() {
        assert_eq!(truncate_content("hi"), "hi");
        let long = "a".repeat(MAX_CONTENT_LEN + 10);
        let cut = truncate_content(&long);
        assert!(cut.ends_with("[truncated]"));
        assert!(cut.len() <= MAX_CONTENT_LEN + "…[truncated]".len());
        // Multibyte boundary: never splits a char.
        let emoji = "é".repeat(MAX_CONTENT_LEN);
        let cut = truncate_content(&format!("{emoji}x"));
        assert!(cut.ends_with("[truncated]"));
    }

    #[tokio::test]
    async fn disabled_never_sends() {
        // No token/channel: early return, nothing spawned.
        Alerter::disabled().send("hi".to_string());
        Alerter::new().send("hi".to_string());
        Alerter::new()
            .with_token("test-token-fixture")
            .send("hi".to_string());
    }

    #[tokio::test]
    async fn delivery_paths_do_not_panic() {
        // Connection-refused loopback: the error path only logs.
        // (No external network: nothing listens on port 9.)
        let alert = Alerter::with_api_base("http://127.0.0.1:9")
            .with_token("test-token-fixture")
            .with_channel("123456789");
        alert.send("hi".to_string());
        // Let detached tasks run; assertions are "no panic, returns fast".
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    #[test]
    fn halt_card_and_breaker_line() {
        let card = Alerter::format_halt(&PAPER, 10.0, -12.34);
        assert!(card.contains("[DAY-LOSS HALT]"));
        assert!(card.contains("`paper · adaptive`"));
        assert!(card.contains("-$10.00/day"));
        assert!(card.contains("$-12.34"));
        assert!(card.contains("exits always work"));
        assert_eq!(Alerter::breaker_line(false, 0.0), "`armed`");
        assert_eq!(
            Alerter::breaker_line(true, -12.34),
            "`HALTED (day $-12.34)`"
        );
    }
}

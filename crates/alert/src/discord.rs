//! Hermes CLI dispatch plus Discord markdown card formatters.

use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tracing::warn;

/// Per-message dispatch deadline (failure tolerance per skill).
pub const DISPATCH_TIMEOUT: Duration = Duration::from_secs(5);
/// Hermes binary invoked for delivery.
pub const HERMES_BIN: &str = "hermes";
/// Env override for the Hermes binary path. Prefer an absolute path here:
/// a bare `"hermes"` resolves via `PATH`, and a writable directory early
/// in `PATH` lets an attacker substitute the alert binary.
pub const HERMES_BIN_ENV: &str = "HERMES_BIN";
/// Maximum concurrent Hermes children: every `send` forks a process, so
/// an exit storm must queue behind permits instead of fork-bombing the
/// host. Excess messages are dropped with a counter (see
/// [`Alerter::dropped_count`]) — alerts are observability, never consensus.
pub const MAX_INFLIGHT_ALERTS: usize = 4;

/// Fire-and-forget Discord alerter. Cloneable and cheap.
#[derive(Debug, Clone)]
pub struct Alerter {
    binary: String,
    enabled: bool,
    timeout: Duration,
    /// Optional Hermes channel target. `None` preserves the skill's
    /// exact argv (`send --to discord <msg>`), leaving channel routing
    /// to the host Hermes configuration. `Some(id)` sends to
    /// `--to discord:<id>` (confirmed Hermes syntax for a specific
    /// channel). Channel IDs are numeric snowflakes; anything else is
    /// rejected at set time (fail-closed, keeps the default target).
    channel: Option<String>,
    /// Dispatch permits shared across clones (see [`MAX_INFLIGHT_ALERTS`]).
    permits: Arc<Semaphore>,
    /// Messages dropped for lack of a permit (shared across clones).
    dropped: Arc<std::sync::atomic::AtomicU64>,
}

/// Heartbeat payload: counters + cash snapshot, bundled so new
/// fields cannot grow the formatter arity again.
pub struct HeartbeatReport<'a> {
    /// `live` or `paper`.
    pub mode: &'a str,
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
    /// Free paper cash (USD).
    pub cash_usd: f64,
    /// Mark-to-market paper equity (USD).
    pub equity_usd: f64,
}

/// Resolve the Hermes binary: explicit override first, `HERMES_BIN` env
/// second, bare `"hermes"` on `PATH` last.
fn resolve_binary() -> String {
    match std::env::var(HERMES_BIN_ENV) {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => HERMES_BIN.to_string(),
    }
}

impl Default for Alerter {
    fn default() -> Self {
        Self::new()
    }
}

impl Alerter {
    /// Enabled alerter shelling out to Hermes (see [`resolve_binary`]).
    pub fn new() -> Self {
        Self {
            binary: resolve_binary(),
            enabled: true,
            timeout: DISPATCH_TIMEOUT,
            channel: None,
            permits: Arc::new(Semaphore::new(MAX_INFLIGHT_ALERTS)),
            dropped: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// No-op alerter: [`Alerter::send`] returns immediately.
    pub fn disabled() -> Self {
        Self {
            binary: resolve_binary(),
            enabled: false,
            timeout: DISPATCH_TIMEOUT,
            channel: None,
            permits: Arc::new(Semaphore::new(MAX_INFLIGHT_ALERTS)),
            dropped: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Messages dropped for lack of a dispatch permit (storm backpressure).
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Override the binary (tests point this at `echo`/`false`).
    pub fn with_binary(mut self, binary: &str) -> Self {
        self.binary = binary.to_string();
        self
    }

    /// Route to a specific Hermes Discord channel (`--to
    /// `discord:<channel-id>`). Non-numeric input is rejected (keeps the
    /// default `discord` target) since channel IDs are numeric snowflakes.
    /// Overlong input is rejected too (argv bloat fails the spawn).
    pub fn with_channel(mut self, channel: &str) -> Self {
        let channel = channel.trim();
        if !channel.is_empty()
            && channel.len() <= 32
            && channel.bytes().all(|b| b.is_ascii_digit())
        {
            self.channel = Some(channel.to_string());
        }
        self
    }

    /// argv for one dispatch (pure, unit-tested): the skill's exact
    /// `send --to discord <msg>`, or `send --to discord:<id> <msg>` when
    /// a channel is configured. The `--to` value is assembled here (never
    /// through `sanitize`, which strips `:`).
    fn argv(&self, message: &str) -> Vec<String> {
        let target = match &self.channel {
            Some(id) => format!("discord:{id}"),
            None => "discord".to_string(),
        };
        vec!["send".to_string(), "--to".to_string(), target, message.to_string()]
    }

    /// Dispatch `message` as a detached task. Never blocks, never fails
    /// the caller: delivery problems only log. At most
    /// [`MAX_INFLIGHT_ALERTS`] children run at once; excess messages are
    /// dropped with a counter rather than forking unboundedly.
    ///
    /// Pass formatter output (`format_entry`, ...): the message travels
    /// as a positional argv element after `--to`, so a payload starting
    /// with `-` could parse as a Hermes flag. Formatters sanitize their
    /// fields; raw caller strings are not re-sanitized here (no shell is
    /// involved — `Command` execs directly — but flag injection is a
    /// parser property of Hermes, whose `--` support is unverified).
    pub fn send(&self, message: String) {
        if !self.enabled {
            return;
        }
        let permit = match self.permits.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                let dropped = self.dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                warn!(target: "alerting", dropped, "alert storm; dropping message");
                return;
            }
        };
        let binary = self.binary.clone();
        let timeout = self.timeout;
        let args = self.argv(&message);
        tokio::spawn(async move {
            // Hold the permit for the whole child lifetime.
            let _permit = permit;
            let mut cmd = Command::new(&binary);
            // A timed-out future drops here: kill the orphan so a late
            // child cannot deliver a duplicate after the timeout.
            cmd.kill_on_drop(true);
            let run = cmd.args(&args).output();
            match tokio::time::timeout(timeout, run).await {
                Err(_) => warn!(target: "alerting", "Hermes alert timed out"),
                Ok(Err(e)) => warn!(target: "alerting", "Failed to spawn Hermes alert: {e}"),
                Ok(Ok(output)) if !output.status.success() => {
                    let err = String::from_utf8_lossy(&output.stderr);
                    let err: String = err.chars().take(1024).collect();
                    warn!(target: "alerting", "Hermes Discord delivery failed: {err}");
                }
                Ok(Ok(_)) => {}
            }
        });
    }

    /// Entry card (skill format, verbatim).
    pub fn format_entry(
        mint: &str,
        entry_usd: f64,
        tp_pct: f64,
        sl_pct: f64,
        notional_usd: f64,
    ) -> String {
        let mint = Self::sanitize(mint);
        format!(
            "🟢 **[TRADE OPENED]**\n\
             • **Token Mint:** `{mint}`\n\
             • **Entry Price:** `${entry_usd:.8}`\n\
             • **Size:** `${notional_usd:.2}`\n\
             • **Target TP:** `+{tp_pct:.1}%` | **Stop Loss:** `{sl_pct:.1}%`\n\
             • **Explorer:** https://explorer.solana.com/address/{mint}"
        )
    }

    /// Strip markdown/Discord control characters from interpolated fields.
    /// Current callers only pass base58 pubkeys, bundle UUIDs, and internal
    /// enums (all inert today), but any future field (token name/symbol)
    /// is attacker-controlled — sanitize at the boundary so a new card
    /// cannot become a mention/embed injection. Brackets and parens go
    /// too (`[text](url)` links), as do Unicode format characters
    /// (zero-width/bidi overrides, which `char::is_control` misses).
    fn sanitize(s: &str) -> String {
        s.chars()
            .filter(|c| {
                !matches!(
                    c,
                    '`' | '*' | '_' | '~' | '|' | '>' | '@' | '#' | ':' | '/' | '\\' | '['
                        | ']' | '(' | ')'
                ) && !c.is_control()
                    && !is_format_char(*c)
            })
            .take(128)
            .collect()
    }

    /// Exit card (skill format, verbatim).
    pub fn format_exit(
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
             • **Token Mint:** `{mint}`\n\
             • **Exit Reason:** `{reason}`\n\
             • **Realized PnL:** `${net_pnl:+.2}` (`{pct:+.2}%`)\n\
             • **Portfolio Equity:** `${equity:.2}`"
        )
    }

    /// Live buy dispatch card (bundle UUID for tracking).
    pub fn format_live_buy(mint: &str, bundle_id: &str, amount_tokens: u64) -> String {
        let mint = Self::sanitize(mint);
        let bundle_id = Self::sanitize(bundle_id);
        format!(
            "🚀 **[LIVE BUY SENT]**\n\
             • **Token Mint:** `{mint}`\n\
             • **Amount:** `{amount_tokens}` base units\n\
             • **Bundle:** `{bundle_id}`\n\
             • **Explorer:** https://explorer.solana.com/address/{mint}"
        )
    }

    /// Live sell dispatch card.
    pub fn format_live_sell(mint: &str, bundle_id: &str) -> String {
        let mint = Self::sanitize(mint);
        let bundle_id = Self::sanitize(bundle_id);
        format!(
            "🔻 **[LIVE SELL SENT]**\n\
             • **Token Mint:** `{mint}`\n\
             • **Bundle:** `{bundle_id}`\n\
             • **Explorer:** https://explorer.solana.com/address/{mint}"
        )
    }

    /// Migration card: destination plus pool addresses when resolved.
    /// `amm_id` / `base_mint` / `quote_mint` render as `unknown` when absent.
    pub fn format_migration(
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
        let amm = amm_id.map(Self::sanitize).unwrap_or_else(|| "unknown".to_string());
        let base = base_mint.map(Self::sanitize).unwrap_or_else(|| "unknown".to_string());
        let quote = quote_mint.map(Self::sanitize).unwrap_or_else(|| "unknown".to_string());
        format!(
            "🌉 **[MIGRATION DETECTED]**\n\
             • **Token Mint:** `{mint}`\n\
             • **Destination:** `{destination}`\n\
             • **AMM:** `{}`\n\
             • **Base Mint:** `{}`\n\
             • **Quote Mint:** `{}`\n\
             • **Tx:** https://explorer.solana.com/tx/{signature}",
            amm, base, quote,
        )
    }

    /// Heartbeat card: periodic proof-of-life for quiet markets. All
    /// inputs are counters (no addresses), so nothing needs sanitizing
    /// beyond the mode label.
    pub fn format_heartbeat(r: &HeartbeatReport<'_>) -> String {
        let mode = Self::sanitize(r.mode);
        let uptime_h = r.uptime_secs / 3_600;
        let uptime_m = (r.uptime_secs % 3_600) / 60;
        format!(
            "💚 **[BOT HEARTBEAT]**\n\
             • **Mode:** `{mode}`\n\
             • **Uptime:** `{uptime_h}h {uptime_m}m`\n\
             • **Logs seen:** `{}`\n\
             • **Candidates:** `{}`\n\
             • **Parsed:** `{}`\n\
             • **Dropped:** `{}`\n\
             • **Watchlist:** `{}` pending\n\
             • **Open:** `{}` position(s)\n\
             • **Cash:** `${:.2}`\n\
             • **Equity:** `${:.2}`",
            r.logs_seen,
            r.candidates,
            r.parse_ok,
            r.dropped,
            r.watchlist_pending,
            r.open_positions,
            r.cash_usd,
            r.equity_usd,
        )
    }

    /// Post-migration spike card: price crossed `multiple` off the first
    /// pool tick after migration.
    pub fn format_follow_spike(mint: &str, multiple: f64, price_usd: f64) -> String {
        let mint = Self::sanitize(mint);
        format!(
            "🚀 **[POST-MIGRATION SPIKE]**\n\
             • **Token Mint:** `{mint}`\n\
             • **Move:** `{multiple:.1}x off migration`\n\
             • **Price:** `${price_usd:.8}`\n\
             • **Explorer:** https://explorer.solana.com/address/{mint}"
        )
    }
}

/// Unicode format characters (`Cf`) smuggled past `char::is_control`:
/// zero-width spaces/joiners and bidi overrides that reorder displayed
/// text (spoofed mints, fake mentions).
fn is_format_char(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_card_marks_fields() {
        let card = Alerter::format_entry("MINT", 0.00000420, 40.0, -10.0, 10.0);
        assert!(card.contains("[TRADE OPENED]"));
        assert!(card.contains("`MINT`"));
        assert!(card.contains("$0.00000420"));
        assert!(card.contains("+40.0%"));
        assert!(card.contains("-10.0%"));
        assert!(card.contains("explorer.solana.com/address/MINT"));
    }

    #[test]
    fn exit_card_signs_and_tags() {
        let win = Alerter::format_exit("M", "TakeProfit", 4.2, 42.0, 104.2, false);
        assert!(win.contains("🎯"));
        assert!(win.contains("POSITION CLOSED"));
        assert!(win.contains("$+4.20"));
        let loss = Alerter::format_exit("M", "StopLoss", -1.5, -15.0, 98.5, false);
        assert!(loss.contains("🛑"));
        let moon = Alerter::format_exit("M", "Manual", 0.0, 0.0, 100.0, true);
        assert!(moon.contains("MOONBAG PARTIAL EXIT"));
    }

    #[test]
    fn follow_spike_card_marks_multiple() {
        let card = Alerter::format_follow_spike("M", 2.0, 0.01);
        assert!(card.contains("[POST-MIGRATION SPIKE]"));
        assert!(card.contains("2.0x"));
        assert!(card.contains("explorer.solana.com/address/M"));
    }

    #[test]
    fn heartbeat_card_marks_counters() {
        let card = Alerter::format_heartbeat(&HeartbeatReport {
            mode: "live",
            uptime_secs: 7_500,
            logs_seen: 1000,
            candidates: 50,
            parse_ok: 48,
            dropped: 2,
            watchlist_pending: 3,
            open_positions: 1,
            cash_usd: 90.5,
            equity_usd: 102.25,
        });
        assert!(card.contains("[BOT HEARTBEAT]"));
        assert!(card.contains("`live`"));
        assert!(card.contains("2h 5m"));
        assert!(card.contains("`1000`"));
        assert!(card.contains("$90.50"));
        assert!(card.contains("$102.25"));
    }

    #[test]
    fn migration_card_marks_pool() {
        let card =
            Alerter::format_migration("M", "RaydiumV4", "sig", Some("AMM"), Some("BASE"), None);
        assert!(card.contains("[MIGRATION DETECTED]"));
        assert!(card.contains("RaydiumV4"));
        assert!(card.contains("`AMM`"));
        assert!(card.contains("explorer.solana.com/tx/sig"));
        let bare = Alerter::format_migration("M", "PumpSwap", "sig", None, None, None);
        assert!(bare.contains("unknown"));
    }

    #[test]
    fn interpolated_fields_are_sanitized() {
        // Attacker-shaped inputs must not survive into the card as
        // markdown mentions, code fences, or links (template backticks
        // around fields are by design; payload backticks must be gone).
        let card = Alerter::format_exit("@everyone `x`", "TakeProfit", 1.0, 1.0, 1.0, false);
        assert!(!card.contains("@everyone"));
        assert!(!card.contains("`x`"));
        let mig = Alerter::format_migration(
            "M",
            "X",
            "https://evil.example/@everyone",
            Some("`AMM`"),
            None,
            None,
        );
        assert!(!mig.contains('@'));
        assert!(!mig.contains("https://evil.example"));
        // Backtick payloads collapse to plain text inside the template's
        // own code span (no breakout): "`A`M`M`" renders as "`AMM`".
        let tricky = Alerter::format_migration("M", "X", "s", Some("`A`M`M`"), None, None);
        assert!(tricky.contains("`AMM`"));
    }

    #[test]
    fn sanitize_blocks_links_and_bidi() {
        // Markdown links die with their brackets and parens.
        let card = Alerter::format_exit("[click](https://evil.example)", "TakeProfit", 1.0, 1.0, 1.0, false);
        assert!(!card.contains("https://evil.example"));
        assert!(!card.contains("[click]"));
        // Zero-width and bidi-override characters are stripped.
        let sneaky = "AB\u{200B}CD\u{202E}EF";
        assert_eq!(Alerter::sanitize(sneaky), "ABCDEF");
        assert_eq!(Alerter::sanitize("a\u{FEFF}b"), "ab");
    }

    #[test]
    fn storm_drops_behind_permits() {
        // Permit pool is 4: exhaust it, then the next send drops loudly
        // instead of forking.
        let alert = Alerter::new().with_binary("/bin/echo");
        let mut held = Vec::new();
        for _ in 0..MAX_INFLIGHT_ALERTS {
            held.push(alert.permits.clone().try_acquire_owned().expect("permit"));
        }
        alert.send("storm".to_string());
        assert_eq!(alert.dropped_count(), 1);
        drop(held);
    }

    #[test]
    fn channel_rejects_overlong_input() {
        assert_eq!(
            Alerter::new().with_channel(&"1".repeat(64)).argv("hi"),
            vec!["send", "--to", "discord", "hi"]
        );
    }

    #[test]
    fn argv_defaults_to_skill_shape() {
        // No channel: byte-identical to the skill's argv.
        assert_eq!(
            Alerter::new().argv("hi"),
            vec!["send", "--to", "discord", "hi"]
        );
        // Numeric channel id: folded into the --to target.
        assert_eq!(
            Alerter::new().with_channel("123456789").argv("hi"),
            vec!["send", "--to", "discord:123456789", "hi"]
        );
        // Blank or non-numeric input is rejected (default target kept).
        assert_eq!(
            Alerter::new().with_channel("  ").argv("hi"),
            vec!["send", "--to", "discord", "hi"]
        );
        assert_eq!(
            Alerter::new().with_channel("trades;rm -rf").argv("hi"),
            vec!["send", "--to", "discord", "hi"]
        );
    }

    #[tokio::test]
    async fn disabled_never_spawns() {
        // Would fail loudly if executed: proves early return.
        Alerter::disabled()
            .with_binary("/nonexistent-binary-xyz")
            .send("hi".to_string());
    }

    #[tokio::test]
    async fn delivery_paths_do_not_panic() {
        // Exit-code and spawn-failure paths only log.
        Alerter::new()
            .with_binary("/bin/false")
            .send("hi".to_string());
        Alerter::new()
            .with_binary("/nonexistent-binary-xyz")
            .send("hi".to_string());
        Alerter::new()
            .with_binary("/bin/echo")
            .send("hi".to_string());
        // Let detached tasks run; assertions are "no panic, returns fast".
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

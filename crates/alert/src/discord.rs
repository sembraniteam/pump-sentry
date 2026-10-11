//! Hermes CLI dispatch plus Discord markdown card formatters.

use std::time::Duration;
use tokio::process::Command;
use tracing::warn;

/// Per-message dispatch deadline (failure tolerance per skill).
pub const DISPATCH_TIMEOUT: Duration = Duration::from_secs(5);
/// Hermes binary invoked for delivery.
pub const HERMES_BIN: &str = "hermes";

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
}

impl Default for Alerter {
    fn default() -> Self {
        Self::new()
    }
}

impl Alerter {
    /// Enabled alerter shelling out to `hermes`.
    pub fn new() -> Self {
        Self {
            binary: HERMES_BIN.to_string(),
            enabled: true,
            timeout: DISPATCH_TIMEOUT,
            channel: None,
        }
    }

    /// No-op alerter: [`Alerter::send`] returns immediately.
    pub fn disabled() -> Self {
        Self {
            binary: HERMES_BIN.to_string(),
            enabled: false,
            timeout: DISPATCH_TIMEOUT,
            channel: None,
        }
    }

    /// Override the binary (tests point this at `echo`/`false`).
    pub fn with_binary(mut self, binary: &str) -> Self {
        self.binary = binary.to_string();
        self
    }

    /// Route to a specific Hermes Discord channel (`--to
    /// `discord:<channel-id>`). Non-numeric input is rejected (keeps the
    /// default `discord` target) since channel IDs are numeric snowflakes.
    pub fn with_channel(mut self, channel: &str) -> Self {
        let channel = channel.trim();
        if !channel.is_empty() && channel.bytes().all(|b| b.is_ascii_digit()) {
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
    /// the caller: delivery problems only log.
    pub fn send(&self, message: String) {
        if !self.enabled {
            return;
        }
        let binary = self.binary.clone();
        let timeout = self.timeout;
        let args = self.argv(&message);
        tokio::spawn(async move {
            let run = Command::new(&binary).args(&args).output();
            match tokio::time::timeout(timeout, run).await {
                Err(_) => warn!(target: "alerting", "Hermes alert timed out"),
                Ok(Err(e)) => warn!(target: "alerting", "Failed to spawn Hermes alert: {e}"),
                Ok(Ok(output)) if !output.status.success() => {
                    let err = String::from_utf8_lossy(&output.stderr);
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
    /// cannot become a mention/embed injection.
    fn sanitize(s: &str) -> String {
        s.chars()
            .filter(|c| {
                !matches!(c, '`' | '*' | '_' | '~' | '|' | '>' | '@' | '#' | ':' | '/' | '\\')
                    && !c.is_control()
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

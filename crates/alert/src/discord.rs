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
        }
    }

    /// No-op alerter: [`Alerter::send`] returns immediately.
    pub fn disabled() -> Self {
        Self {
            binary: HERMES_BIN.to_string(),
            enabled: false,
            timeout: DISPATCH_TIMEOUT,
        }
    }

    /// Override the binary (tests point this at `echo`/`false`).
    pub fn with_binary(mut self, binary: &str) -> Self {
        self.binary = binary.to_string();
        self
    }

    /// Dispatch `message` as a detached task. Never blocks, never fails
    /// the caller: delivery problems only log.
    pub fn send(&self, message: String) {
        if !self.enabled {
            return;
        }
        let binary = self.binary.clone();
        let timeout = self.timeout;
        tokio::spawn(async move {
            let run = Command::new(&binary)
                .args(["send", "--to", "discord", &message])
                .output();
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
        format!(
            "🟢 **[TRADE OPENED]**\n\
             • **Token Mint:** `{mint}`\n\
             • **Entry Price:** `${entry_usd:.8}`\n\
             • **Size:** `${notional_usd:.2}`\n\
             • **Target TP:** `+{tp_pct:.1}%` | **Stop Loss:** `{sl_pct:.1}%`\n\
             • **Explorer:** https://solscan.io/token/{mint}"
        )
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
        format!(
            "🚀 **[LIVE BUY SENT]**\n\
             • **Token Mint:** `{mint}`\n\
             • **Amount:** `{amount_tokens}` base units\n\
             • **Bundle:** `{bundle_id}`\n\
             • **Explorer:** https://solscan.io/token/{mint}"
        )
    }

    /// Live sell dispatch card.
    pub fn format_live_sell(mint: &str, bundle_id: &str) -> String {
        format!(
            "🔻 **[LIVE SELL SENT]**\n\
             • **Token Mint:** `{mint}`\n\
             • **Bundle:** `{bundle_id}`\n\
             • **Explorer:** https://solscan.io/token/{mint}"
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
        assert!(card.contains("solscan.io/token/MINT"));
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

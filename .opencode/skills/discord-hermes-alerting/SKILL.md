---
name: discord-hermes-alerting
description: Non-blocking Rust integration with Hermes CLI to send rich Discord trading alerts and PnL cards.
license: MIT
compatibility: opencode
metadata:
  domain: notification-ops
  platform: discord
  tool: hermes-cli
---

## Purpose

Enables low-latency, non-blocking dispatch of trade lifecycle alerts (Entry, Take-Profit, Stop-Loss, Liquidation) from
the Rust bot directly into Discord using the pre-configured Hermes CLI.

## Hermes Send Interface

The host system includes Hermes CLI connected to Discord:

```bash
hermes send --to discord "Message content formatted with markdown"
```

## Architectural Invariants (Rust)

1. **Never Block Execution:** Alert dispatch must be spawned as a detached Tokio background task (`tokio::spawn`), never
   awaited inline on the hot pricing or swap paths. At most 4 children run at once (shared semaphore); excess drops
   with a counter — alerts are observability, never consensus.
2. **Failure Tolerance:** If Hermes CLI fails, times out, or Discord returns an error, the error must be logged via
   `tracing::warn!` without affecting portfolio state or trading logic. Timeouts kill the child (`kill_on_drop`)
   so late deliveries cannot duplicate; stderr logs are capped at 1KB. Binary from `HERMES_BIN` env, else `hermes`
   on PATH (prefer absolute — PATH hijack risk).
3. **Markdown Card Standards:** Use Discord-friendly markdown (bold headers, bullet points, emoji tags). Do not use HTML
   or unsupported markdown tables. Sanitize every interpolated field (markdown chars, `[]()` links, zero-width/bidi
   overrides) and cap at 128 chars. Pass formatter output as the positional message (a `-`-leading payload could
   parse as a Hermes flag — `--` support is unverified).

## Rust Implementation Reference

```rust
use tokio::process::Command;
use tracing::warn;

pub struct DiscordAlert;

impl DiscordAlert {
    /// Fire-and-forget alert dispatch (5s timeout, warn-only, child killed
    /// on timeout). Prefer an absolute `HERMES_BIN`.
    pub fn send(message: String) {
        tokio::spawn(async move {
            match Command::new("hermes")
                .args(["send", "--to", "discord", &message])
                .output()
                .await
            {
                Ok(output) if !output.status.success() => {
                    let err = String::from_utf8_lossy(&output.stderr);
                    warn!(target: "alerting", "Hermes Discord alert delivery failed: {err}");
                }
                Err(e) => {
                    warn!(target: "alerting", "Failed to spawn Hermes alert command: {e}");
                }
                _ => {}
            }
        });
    }

    pub fn format_entry(ctx: &CardCtx<'_>, mint: &str, entry_usd: f64, tp_pct: f64, sl_pct: f64, notional_usd: f64) -> String {
        format!(
            "🟢 **[TRADE OPENED]**\n\
             • **Mode:** `paper · adaptive`\n\
             • **Token Mint:** `{mint}`\n\
             • **Entry Price:** `${entry_usd:.8}`\n\
             • **Size:** `${notional_usd:.2}`\n\
             • **Target TP:** `+{tp_pct:.1}%` | **Stop Loss:** `{sl_pct:.1}%`\n\
             • **Explorer:** https://explorer.solana.com/address/{mint}"
        )
    }

    pub fn format_exit(
        ctx: &CardCtx<'_>,
        mint: &str,
        reason: &str,
        net_pnl: f64,
        pct: f64,
        equity: f64,
        is_moonbag: bool,
    ) -> String {
        let icon = if net_pnl >= 0.0 { "🎯" } else { "🛑" };
        let tag = if is_moonbag { "MOONBAG PARTIAL EXIT" } else { "POSITION CLOSED" };
        format!(
            "{icon} **[{tag}]**\n\
             • **Token Mint:** `{mint}`\n\
             • **Exit Reason:** `{reason}`\n\
             • **Realized PnL:** `${net_pnl:+.2}` (`{pct:+.2}%`)\n\
             • **Portfolio Equity:** `${equity:.2}`"
        )
    }
}
```
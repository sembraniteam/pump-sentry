//! Direct Discord delivery (bot token HTTPS) plus card formatters.
//!
//! The Hermes subprocess dispatch is gone: [`Alerter::send_card`] and
//! [`Alerter::send_text`] POST to `POST /channels/{id}/messages` with
//! `Authorization: Bot …`. The token is owned data captured at boot
//! (env-only, scrubbed after capture like the other bearer secrets)
//! and never logged.
//!
//! Cards are the unit of delivery (docs/discord-sidecar.md §5):
//! every message is embed-only. A structured card renders in one embed
//! that mirrors its fields; when the embed would overflow Discord's
//! limits, the card degrades to a description-only embed carrying the
//! same text — embed-style is the wire format, never dual text+embed.

use serde::Serialize;
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
/// Discord embed-description cap: the embed-only degradation path
/// carries the card's text here, truncated with the same marker.
pub const EMBED_DESCRIPTION_MAX: usize = 4096;

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

/// Heartbeat payload: counters plus cash. Also the sidecar status
/// payload (`status.json`, flattened) — plain operator-facing values.
/// `mode`/`strategy` are owned (the writer passes literals; the
/// sidecar deserializes the file), everything else are numbers.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct HeartbeatReport {
    /// `live` or `paper`.
    pub mode: String,
    /// Effective strategy (`adaptive` | `scalp` | `runner`).
    pub strategy: String,
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

// ---------------------------------------------------------------------------
// Embed layer (docs/discord-sidecar.md §5): one text card → one embed.
// ---------------------------------------------------------------------------

/// Embed palette: green entry/fill/win, red halt/loss exit/give-up,
/// yellow warn/attention, grey info/heartbeat.
pub const COLOR_GREEN: u32 = 0x22_C55E;
/// Red: halts, losing exits, live sell dispatch.
pub const COLOR_RED: u32 = 0xEF_4444;
/// Yellow: momentum/attention cards.
pub const COLOR_YELLOW: u32 = 0xF5_9E0B;
/// Grey: informational cards (migration, heartbeat).
pub const COLOR_GREY: u32 = 0x94_A3B8;

/// Discord embed limits (gateway API v10).
pub const EMBED_TITLE_MAX: usize = 256;
/// Discord embed field-name cap.
pub const EMBED_FIELD_NAME_MAX: usize = 256;
/// Discord embed field-value cap.
pub const EMBED_FIELD_VALUE_MAX: usize = 1024;
/// Discord fields-per-embed cap; extras are dropped, never an error.
pub const MAX_EMBED_FIELDS: usize = 25;
/// Approximate total chars per embed (title + footer + fields).
pub const EMBED_TOTAL_MAX: usize = 6000;
/// Field values up to this length render inline (aligned columns);
/// longer values (URLs, annotated counters) take a full row.
const INLINE_VALUE_MAX: usize = 48;

/// One aligned embed column.
#[derive(Debug, Clone, Serialize)]
pub struct EmbedField {
    pub name: String,
    pub value: String,
    pub inline: bool,
}

/// Bottom-right embed caption (the `live · adaptive` mode line).
#[derive(Debug, Clone, Serialize)]
pub struct EmbedFooter {
    pub text: String,
}

/// Structured embed for one card. Serialized straight into the
/// message payload's `embeds` array (always a single element here);
/// the sidecar re-renders it with serenity, hence public fields.
/// Text-shaped payloads (pipeline warnings, degraded cards) ride
/// `description` instead of the placeholder-free `content` fallback:
/// embed-only in, embed-only out.
#[derive(Debug, Clone, Serialize)]
pub struct Embed {
    /// Structured cards always carry a title; text-shaped embeds leave
    /// it empty (skipped — Discord refuses an empty-string title).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub title: String,
    pub color: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub footer: Option<EmbedFooter>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<EmbedField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl Embed {
    /// Approximate rendered size against Discord's 6000-char total.
    /// `false` means the card degrades to a description-only embed
    /// (description carries the truncated text; the send never fails
    /// on limits).
    fn fits(&self) -> bool {
        let fields: usize = self
            .fields
            .iter()
            .map(|f| f.name.len() + f.value.len() + 20)
            .sum();
        let total = self.title.len()
            + self.footer.as_ref().map_or(0, |f| f.text.len())
            + fields
            + self.description.as_ref().map_or(0, |d| d.len());
        total <= EMBED_TOTAL_MAX
    }
}

/// Description-only embed for text payloads (grey by convention).
fn text_embed(description: String) -> Embed {
    Embed {
        title: String::new(),
        color: COLOR_GREY,
        url: None,
        footer: None,
        fields: Vec::new(),
        description: Some(description),
    }
}

/// The alert payload: one authored text rendering plus the single
/// embed the wire receives (embed-only delivery). Plain strings flow
/// through `From` and keep the plain-warning shape as an embed.
#[derive(Debug, Clone)]
pub struct Card {
    /// The card's markdown text (logged/debug view and the degraded
    /// embed's source text; not a wire field anymore).
    pub content: String,
    /// The single embed the message payload actually ships.
    pub embed: Embed,
}

impl From<String> for Card {
    fn from(content: String) -> Self {
        Card {
            content: content.clone(),
            embed: text_embed(truncate_description(&content)),
        }
    }
}

/// Builder for the standard card shape: an `{icon} **[{TAG}]**` header
/// plus `• **Name:** value` bullets, mirrored 1:1 into embed fields.
/// Interpolated data must be sanitized by the caller — the card
/// formatters own that.
#[derive(Debug, Clone)]
pub struct CardBuilder {
    icon: &'static str,
    tag: String,
    title: String,
    color: u32,
    url: Option<String>,
    footer: Option<String>,
    fields: Vec<EmbedField>,
}

impl CardBuilder {
    /// Header tag for the text fallback (e.g. `TRADE OPENED`), plus
    /// the embed title (e.g. `Trade Opened — MINT`).
    pub fn new(icon: &'static str, tag: &str, title: impl Into<String>, color: u32) -> Self {
        Self {
            icon,
            tag: tag.to_string(),
            title: clamp_chars(&title.into(), EMBED_TITLE_MAX),
            color,
            url: None,
            footer: None,
            fields: Vec::new(),
        }
    }

    /// First field and embed footer: the `live · adaptive` mode line
    /// (sanitized here; mode/strategy are operator config, never
    /// secrets).
    pub fn mode(mut self, ctx: &CardCtx<'_>) -> Self {
        let mode = Alerter::sanitize(ctx.mode);
        let strategy = Alerter::sanitize(ctx.strategy);
        self.footer = Some(format!("{mode} · {strategy}"));
        self.field("Mode", format!("`{mode} · {strategy}`"))
    }

    /// One bullet / one embed field. Beyond the 25-field cap, extras
    /// are dropped (the text fallback loses them too — a card is
    /// authored within the limits).
    pub fn field(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        if self.fields.len() >= MAX_EMBED_FIELDS {
            return self;
        }
        let value = value.into();
        self.fields.push(EmbedField {
            name: clamp_chars(&name.into(), EMBED_FIELD_NAME_MAX),
            value: clamp_chars(&value, EMBED_FIELD_VALUE_MAX),
            inline: value.chars().count() <= INLINE_VALUE_MAX,
        });
        self
    }

    /// Deep link (explorer address/tx). Rendered in `content` as well —
    /// pair with a `field("Explorer", …)`.
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    /// Render the text view and the embed-only wire payload: a fitting
    /// structured embed, or a description-only degradation carrying the
    /// same text (never a wire failure, never a dual send).
    pub fn finish(self) -> Card {
        let content = self
            .fields
            .iter()
            .map(|f| format!("• **{}:** {}", f.name, f.value))
            .collect::<Vec<_>>()
            .join("\n");
        let content = format!("{} **[{}]**\n{content}", self.icon, self.tag);
        let footer = self.footer.map(|text| EmbedFooter { text });
        let structured = Embed {
            title: self.title,
            color: self.color,
            url: self.url,
            footer,
            fields: self.fields,
            description: None,
        };
        let embed = if structured.fits() {
            structured
        } else {
            Embed {
                title: String::new(),
                color: self.color,
                url: None,
                footer: None,
                fields: Vec::new(),
                description: Some(truncate_description(&content)),
            }
        };
        Card { content, embed }
    }
}

/// Char-boundary-safe clamp with an ellipsis marker (pure). The
/// marker counts against the cap: the result never exceeds `max` bytes.
fn clamp_chars(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max.saturating_sub('…'.len_utf8());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
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

/// Truncate a message at `max` chars (pure): reserve the honest
/// `…[truncated]` marker inside the cap first (char-boundary walk on
/// the shortened cut), so the output never exceeds `max`. Callers own
/// sanitization, so the marker is safe to append.
fn truncate_at(message: &str, max: usize) -> String {
    const MARKER: &str = "…[truncated]";
    if message.len() <= max {
        return message.to_string();
    }
    let mut end = max.saturating_sub(MARKER.len());
    while !message.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}{}", &message[..end], MARKER)
}

/// Truncate a card/reply to Discord's `content` cap (pure). Public
/// because callers still shape content strings with it.
pub fn truncate_content(message: &str) -> String {
    truncate_at(message, MAX_CONTENT_LEN)
}

/// Truncate an embed description to Discord's cap (pure). Public
/// because the sidecar shapes interaction replies into the same
/// embed-only shape with it.
pub fn truncate_description(message: &str) -> String {
    truncate_at(message, EMBED_DESCRIPTION_MAX)
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

    /// Send plain text as a detached task. Never blocks the caller.
    /// No token or no channel means no-op (boot logs the reason).
    /// Excess load drops with a counter. Embed-only: the text becomes
    /// a grey description embed, the same shape as every other send.
    pub fn send_text(&self, message: String) {
        self.dispatch(text_embed(truncate_description(&message)));
    }

    /// Send a card as a detached task: one embed, no text fallback in
    /// the payload. No token or no channel means no-op (boot logs the
    /// reason). Excess load drops with a counter.
    pub fn send_card(&self, card: Card) {
        self.dispatch(card.embed);
    }

    /// Shared delivery path: acquire a permit, POST one embed-only
    /// payload, log-only failures.
    fn dispatch(&self, embed: Embed) {
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
        tokio::spawn(async move {
            let _permit = permit;
            // Plain serde structs: serialization cannot fail, but a
            // defensive fallback drops the card, never sends a broken
            // payload. Embed-only: `content` never rides the wire.
            let body = match serde_json::to_value(&embed) {
                Ok(value) => serde_json::json!({ "embeds": [value] }),
                Err(e) => {
                    warn!(target: "alerting", error = %e, "embed serialization failed; card dropped");
                    return;
                }
            };
            let client = reqwest::Client::new();
            let run = client
                .post(&url)
                .header("Authorization", format!("Bot {token}"))
                .json(&body)
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
    ) -> Card {
        let mint = Self::sanitize(mint);
        let risk = Self::sanitize(risk);
        let explorer = format!("{EXPLORER_ADDRESS}/{mint}");
        CardBuilder::new(
            "🟢",
            "TRADE OPENED",
            format!("Trade Opened — {mint}"),
            COLOR_GREEN,
        )
        .mode(ctx)
        .field("Token Mint", format!("`{mint}`"))
        .field("Entry Price", format!("`${entry_usd:.8}`"))
        .field("Size", format!("`${notional_usd:.2}`"))
        .field("Target TP", format!("`+{tp_pct:.1}%`"))
        .field("Stop Loss", format!("`{sl_pct:.1}%`"))
        .field("Risk", format!("`{risk}`"))
        .field("Explorer", explorer.clone())
        .url(explorer)
        .finish()
    }

    /// Day-loss halt card. Fires once per UTC day (see
    /// `DailyBreaker::halt_state`); the heartbeat carries the state
    /// after that. Exits always work — only entries halt.
    pub fn format_halt(ctx: &CardCtx<'_>, cap_usd: f64, day_net: f64) -> Card {
        CardBuilder::new("🛑", "DAY-LOSS HALT", "Day-Loss Halt", COLOR_RED)
            .mode(ctx)
            .field("Cap", format!("`-${cap_usd:.2}/day`"))
            .field("Day Net", format!("`${day_net:+.2}`"))
            .field("Note", "entries halted; exits always work")
            .finish()
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

    /// Exit card. Green on a win, red on a loss (doc §5 palette).
    pub fn format_exit(
        ctx: &CardCtx<'_>,
        mint: &str,
        reason: &str,
        net_pnl: f64,
        pct: f64,
        equity: f64,
        is_moonbag: bool,
    ) -> Card {
        let mint = Self::sanitize(mint);
        let reason = Self::sanitize(reason);
        let win = net_pnl >= 0.0;
        let (tag, title) = if is_moonbag {
            (
                "MOONBAG PARTIAL EXIT",
                format!("Moonbag Partial Exit — {mint}"),
            )
        } else {
            ("POSITION CLOSED", format!("Position Closed — {mint}"))
        };
        CardBuilder::new(
            if win { "🎯" } else { "🛑" },
            tag,
            title,
            if win { COLOR_GREEN } else { COLOR_RED },
        )
        .mode(ctx)
        .field("Token Mint", format!("`{mint}`"))
        .field("Exit Reason", format!("`{reason}`"))
        .field("Realized PnL", format!("`${net_pnl:+.2}` (`{pct:+.2}%`)"))
        .field("Portfolio Equity", format!("`${equity:.2}`"))
        .finish()
    }

    /// Live buy card (bundle id for tracking).
    pub fn format_live_buy(
        ctx: &CardCtx<'_>,
        mint: &str,
        bundle_id: &str,
        amount_tokens: u64,
    ) -> Card {
        let mint = Self::sanitize(mint);
        let bundle_id = Self::sanitize(bundle_id);
        let explorer = format!("{EXPLORER_ADDRESS}/{mint}");
        CardBuilder::new(
            "🚀",
            "LIVE BUY SENT",
            format!("Live Buy Sent — {mint}"),
            COLOR_GREEN,
        )
        .mode(ctx)
        .field("Token Mint", format!("`{mint}`"))
        .field("Amount", format!("`{amount_tokens}` base units"))
        .field("Bundle", format!("`{bundle_id}`"))
        .field("Explorer", explorer.clone())
        .url(explorer)
        .finish()
    }

    /// Live sell dispatch card.
    pub fn format_live_sell(ctx: &CardCtx<'_>, mint: &str, bundle_id: &str) -> Card {
        let mint = Self::sanitize(mint);
        let bundle_id = Self::sanitize(bundle_id);
        let explorer = format!("{EXPLORER_ADDRESS}/{mint}");
        CardBuilder::new(
            "🔻",
            "LIVE SELL SENT",
            format!("Live Sell Sent — {mint}"),
            COLOR_RED,
        )
        .mode(ctx)
        .field("Token Mint", format!("`{mint}`"))
        .field("Bundle", format!("`{bundle_id}`"))
        .field("Explorer", explorer.clone())
        .url(explorer)
        .finish()
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
    ) -> Card {
        let mint = Self::sanitize(mint);
        let destination = Self::sanitize(destination);
        let signature = Self::sanitize(signature);
        let unknown = "unknown";
        let amm = amm_id.map(Self::sanitize).unwrap_or(unknown.to_string());
        let base = base_mint.map(Self::sanitize).unwrap_or(unknown.to_string());
        let quote = quote_mint
            .map(Self::sanitize)
            .unwrap_or(unknown.to_string());
        let tx = format!("{EXPLORER_TX}/{signature}");
        CardBuilder::new(
            "🌉",
            "MIGRATION DETECTED",
            format!("Migration Detected — {mint}"),
            COLOR_GREY,
        )
        .mode(ctx)
        .field("Token Mint", format!("`{mint}`"))
        .field("Destination", format!("`{destination}`"))
        .field("AMM", format!("`{amm}`"))
        .field("Base Mint", format!("`{base}`"))
        .field("Quote Mint", format!("`{quote}`"))
        .field("Tx", tx.clone())
        .url(tx)
        .finish()
    }

    /// Heartbeat card. Only mode/strategy need sanitize (rest are numbers).
    pub fn format_heartbeat(r: &HeartbeatReport) -> Card {
        let mode = Self::sanitize(&r.mode);
        let strategy = Self::sanitize(&r.strategy);
        let uptime_h = r.uptime_secs / 3_600;
        let uptime_m = (r.uptime_secs % 3_600) / 60;
        let oldest_h = r.oldest_open_age_secs / 3_600;
        let oldest_m = (r.oldest_open_age_secs % 3_600) / 60;
        let data = if r.data_paused {
            "`DEGRADED — entries paused`"
        } else {
            "`ok`"
        };
        CardBuilder::new("💚", "BOT HEARTBEAT", "Bot Heartbeat", COLOR_GREY)
            .field("Mode", format!("`{mode} · {strategy}`"))
            .field("Uptime", format!("`{uptime_h}h {uptime_m}m`"))
            .field("Logs seen", format!("`{}`", r.logs_seen))
            .field("Candidates", format!("`{}`", r.candidates))
            .field("Parsed", format!("`{}`", r.parse_ok))
            .field("Dropped", format!("`{}`", r.dropped))
            .field("Watchlist", format!("`{}` pending", r.watchlist_pending))
            .field("Open", format!("`{}` position(s)", r.open_positions))
            .field("Oldest open", format!("`{oldest_h}h {oldest_m}m`"))
            .field("Manual bags", format!("`{}` held", r.manual_bags))
            .field(
                "Diverged",
                format!("`{}` (paper without live cover)", r.diverged),
            )
            .field("Pending buys", format!("`{}` unconfirmed", r.pending_buys))
            .field("Breaker", Self::breaker_line(r.breaker_halted, r.day_net))
            .field("Data", data)
            .field(
                "RPC",
                format!(
                    "`{}/{} ok · failovers {}{}`",
                    r.rpc_healthy,
                    r.rpc_endpoints,
                    r.rpc_failover_reads,
                    if r.rpc_inconsistent {
                        " · INCONSISTENT"
                    } else {
                        ""
                    }
                ),
            )
            .field(
                "Stream gaps shed",
                format!(
                    "`{}` slots (`{}` capped backfills)",
                    r.stream_gaps_shed, r.stream_pages_capped
                ),
            )
            .field("GiveUps", format!("`{}` in window", r.giveups_window))
            .field("Cash", format!("`${:.2}`", r.cash_usd))
            .field("Equity", format!("`${:.2}`", r.equity_usd))
            .finish()
    }

    /// Breaker line: `armed`, or `HALTED` with the day net that
    /// tripped it. Plain values, no secrets. Public because the
    /// sidecar's compact status renders the same breaker wording.
    pub fn breaker_line(halted: bool, day_net: f64) -> String {
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
    ) -> Card {
        let mint = Self::sanitize(mint);
        let explorer = format!("{EXPLORER_ADDRESS}/{mint}");
        CardBuilder::new(
            "🚀",
            "POST-MIGRATION SPIKE",
            format!("Post-Migration Spike — {mint}"),
            COLOR_YELLOW,
        )
        .mode(ctx)
        .field("Token Mint", format!("`{mint}`"))
        .field("Move", format!("`{multiple:.1}x` off migration"))
        .field("Price", format!("`${price_usd:.8}`"))
        .field("Explorer", explorer.clone())
        .url(explorer)
        .finish()
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
    fn construct_heartbeat_fixture() -> HeartbeatReport {
        HeartbeatReport {
            mode: "live".to_string(),
            strategy: "runner".to_string(),
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
        assert!(card.content.contains("[TRADE OPENED]"));
        assert!(card.content.contains("`MINT`"));
        assert!(card.content.contains("$0.00000420"));
        assert!(card.content.contains("+40.0%"));
        assert!(card.content.contains("-10.0%"));
        assert!(card.content.contains("explorer.solana.com/address/MINT"));
        assert!(card.content.contains("`paper · adaptive`"));
        let embed = &card.embed;
        assert_eq!(embed.title, "Trade Opened — MINT");
        assert_eq!(embed.color, COLOR_GREEN);
        assert_eq!(
            embed.url.as_deref(),
            Some("https://explorer.solana.com/address/MINT")
        );
        assert_eq!(embed.fields.len(), 8);
        assert_eq!(embed.fields[0].name, "Mode");
        assert_eq!(embed.fields[0].value, "`paper · adaptive`");
        assert!(embed.fields[0].inline);
        // Inline rule is value length: short values are columns, long
        // ones take a full row (Explorer URLs with real 44-char mints).
        assert_eq!(
            embed.fields[7].inline,
            embed.fields[7].value.chars().count() <= INLINE_VALUE_MAX
        );
        // Footer mirrors the mode line (no `Mode:` label, no backticks).
        assert_eq!(
            embed.footer.as_ref().map(|f| &*f.text),
            Some("paper · adaptive")
        );
    }

    #[test]
    fn exit_card_signs_and_tags() {
        let win = Alerter::format_exit(&PAPER, "M", "TakeProfit", 4.2, 42.0, 104.2, false);
        assert!(win.content.contains("🎯"));
        assert!(win.content.contains("POSITION CLOSED"));
        assert!(win.content.contains("$+4.20"));
        assert_eq!(win.embed.color, COLOR_GREEN);
        assert_eq!(win.embed.title, "Position Closed — M");
        let loss = Alerter::format_exit(&PAPER, "M", "StopLoss", -1.5, -15.0, 98.5, false);
        assert!(loss.content.contains("🛑"));
        assert_eq!(loss.embed.color, COLOR_RED);
        let moon = Alerter::format_exit(&PAPER, "M", "Manual", 0.0, 0.0, 100.0, true);
        assert!(moon.content.contains("MOONBAG PARTIAL EXIT"));
        assert_eq!(moon.embed.title, "Moonbag Partial Exit — M");
    }

    #[test]
    fn follow_spike_card_marks_multiple() {
        let card = Alerter::format_follow_spike(&PAPER, "M", 2.0, 0.01);
        assert!(card.content.contains("[POST-MIGRATION SPIKE]"));
        assert!(card.content.contains("2.0x"));
        assert!(card.content.contains("explorer.solana.com/address/M"));
        let embed = &card.embed;
        assert_eq!(embed.color, COLOR_YELLOW);
        assert_eq!(embed.title, "Post-Migration Spike — M");
    }

    #[test]
    fn heartbeat_card_marks_counters() {
        let card = Alerter::format_heartbeat(&HeartbeatReport {
            mode: "live".to_string(),
            strategy: "runner".to_string(),
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
        assert!(card.content.contains("[BOT HEARTBEAT]"));
        assert!(card.content.contains("`live · runner`"));
        assert!(card.content.contains("2h 5m"));
        assert!(card.content.contains("`1000`"));
        assert!(card.content.contains("`2` held"));
        assert!(card.content.contains("$90.50"));
        assert!(card.content.contains("$102.25"));
        // Observability lines: health, RPC pool, stream gaps, failures.
        assert!(card.content.contains("**Data:** `ok`"));
        assert!(card.content.contains("**RPC:** `2/2 ok · failovers 3`"));
        assert!(card.content.contains("**Oldest open:** `1h 13m`"));
        assert!(
            card.content
                .contains("**Stream gaps shed:** `7` slots (`1` capped backfills)")
        );
        assert!(card.content.contains("**GiveUps:** `1` in window"));
        // 19 fields: within the 25-field cap, and under the total cap.
        let embed = &card.embed;
        assert_eq!(embed.fields.len(), 19);
        assert_eq!(embed.color, COLOR_GREY);
        assert!(embed.footer.is_none());
        // Degraded state reads as paused, inconsistent as a flag.
        let degraded = Alerter::format_heartbeat(&HeartbeatReport {
            data_paused: true,
            rpc_healthy: 0,
            rpc_inconsistent: true,
            ..construct_heartbeat_fixture()
        });
        assert!(
            degraded
                .content
                .contains("**Data:** `DEGRADED — entries paused`")
        );
        assert!(
            degraded
                .content
                .contains("`0/2 ok · failovers 3 · INCONSISTENT`")
        );
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
        assert!(card.content.contains("[MIGRATION DETECTED]"));
        assert!(card.content.contains("RaydiumV4"));
        assert!(card.content.contains("`AMM`"));
        assert!(card.content.contains("explorer.solana.com/tx/sig"));
        let embed = &card.embed;
        assert_eq!(embed.color, COLOR_GREY);
        assert_eq!(
            embed.url.as_deref(),
            Some("https://explorer.solana.com/tx/sig")
        );
        let bare = Alerter::format_migration(&LIVE, "M", "PumpSwap", "sig", None, None, None);
        assert!(bare.content.contains("unknown"));
    }

    #[test]
    fn interpolated_fields_are_sanitized() {
        // Attacker-shaped inputs must not survive as mentions, fences, or links.
        let card =
            Alerter::format_exit(&PAPER, "@everyone `x`", "TakeProfit", 1.0, 1.0, 1.0, false);
        assert!(!card.content.contains("@everyone"));
        assert!(!card.content.contains("`x`"));
        let mig = Alerter::format_migration(
            &PAPER,
            "M",
            "X",
            "https://evil.example/@everyone",
            Some("`AMM`"),
            None,
            None,
        );
        assert!(!mig.content.contains('@'));
        assert!(!mig.content.contains("https://evil.example"));
        // Backtick payloads collapse to plain text inside the template's own span.
        let tricky = Alerter::format_migration(&PAPER, "M", "X", "s", Some("`A`M`M`"), None, None);
        assert!(tricky.content.contains("`AMM`"));
        // The embed carries the same sanitized values — no side channel
        // for mentions or link syntax (same predicates as the content).
        let embed = &mig.embed;
        assert!(
            !embed
                .fields
                .iter()
                .any(|f| f.value.contains('@') || f.value.contains("https://evil.example"))
        );
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
        assert!(!card.content.contains("https://evil.example"));
        assert!(!card.content.contains("[click]"));
        // Zero-width and bidi-override characters are stripped.
        let sneaky = "AB\u{200B}CD\u{202E}EF";
        assert_eq!(Alerter::sanitize(sneaky), "ABCDEF");
        assert_eq!(Alerter::sanitize("a\u{FEFF}b"), "ab");
    }

    #[test]
    fn plain_text_card_rides_a_description_embed() {
        let card = Card::from("raw pipeline warning".to_string());
        assert_eq!(card.content, "raw pipeline warning");
        let embed = card.embed;
        assert_eq!(embed.title, "");
        assert_eq!(embed.color, COLOR_GREY);
        assert!(embed.fields.is_empty());
        assert_eq!(embed.description.as_deref(), Some("raw pipeline warning"));
        // Embed-only wire: empty title must not serialize at all.
        let value = serde_json::to_value(&embed).expect("embed serializes");
        assert!(value.get("title").is_none());
        assert_eq!(value["description"], "raw pipeline warning");
    }

    #[test]
    fn embed_serializes_into_message_shape() {
        // The dispatch body: exactly one `embeds` entry, no `content`.
        let card = Alerter::format_halt(&PAPER, 10.0, -12.34);
        let value = serde_json::to_value(&card.embed).expect("plain struct serializes");
        assert_eq!(value["title"], "Day-Loss Halt");
        assert_eq!(value["color"], COLOR_RED);
        assert_eq!(value["footer"]["text"], "paper · adaptive");
        assert_eq!(value["fields"][0]["name"], "Mode");
        assert_eq!(value["fields"][0]["inline"], true);
        // No url on halt cards: the key is omitted entirely.
        assert!(value.get("url").is_none());
        // Structured cards carry no description body.
        assert!(value.get("description").is_none());
    }

    #[test]
    fn embed_limits_clamp_and_drop() {
        // Field name/value clamp at Discord's caps with an ellipsis.
        let card = CardBuilder::new("🧪", "TEST", "Test", COLOR_GREY)
            .field("N", "v".repeat(EMBED_FIELD_VALUE_MAX + 50))
            .field("n".repeat(EMBED_FIELD_NAME_MAX + 50), "x")
            .finish();
        let embed = &card.embed;
        assert_eq!(embed.fields[0].value.len(), EMBED_FIELD_VALUE_MAX); // incl. ellipsis
        assert!(embed.fields[0].value.ends_with('…'));
        assert_eq!(embed.fields[1].name.len(), EMBED_FIELD_NAME_MAX);
        // Beyond 25 fields, extras are dropped.
        let mut many = CardBuilder::new("🧪", "TEST", "Test", COLOR_GREY);
        for i in 0..MAX_EMBED_FIELDS + 5 {
            many = many.field(format!("f{i}"), "v");
        }
        assert_eq!(many.clone().finish().embed.fields.len(), MAX_EMBED_FIELDS);
        // Overflowing totals degrade to a description-only embed:
        // embed-only wire, never broken sends.
        // (The title self-clamps, so drive the total through fields.)
        let mut huge = CardBuilder::new("🧪", "TEST", "Test", COLOR_GREY);
        for _ in 0..MAX_EMBED_FIELDS {
            huge = huge.field("field", "v".repeat(240));
        }
        let huge = huge.finish();
        // Embed-only degradation: an overflowing structured embed
        // degrades to a description-only embed with the same palette —
        // never removed, never sent dual.
        assert!(huge.embed.fields.is_empty());
        assert!(huge.embed.title.is_empty());
        assert_eq!(huge.embed.color, COLOR_GREY);
        let description = huge.embed.description.expect("degraded description");
        assert!(description.contains("[TEST]"));
        assert!(description.ends_with("…[truncated]"));
        assert!(description.len() <= EMBED_DESCRIPTION_MAX);
        assert!(huge.content.contains("[TEST]"));
    }

    #[test]
    fn clamp_chars_respects_char_boundaries() {
        let emoji = "é".repeat(EMBED_FIELD_VALUE_MAX + 10);
        let clamped = clamp_chars(&emoji, EMBED_FIELD_VALUE_MAX);
        assert!(clamped.ends_with('…'));
        // Never over the cap and never splits a char.
        assert!(clamped.len() <= EMBED_FIELD_VALUE_MAX);
        assert!(clamped.is_char_boundary(clamped.len() - '…'.len_utf8()));
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
        alert.send_text("storm".to_string());
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
        assert!(cut.len() <= MAX_CONTENT_LEN);
        // Multibyte boundary: never splits a char.
        let emoji = "é".repeat(MAX_CONTENT_LEN);
        let cut = truncate_content(&format!("{emoji}x"));
        assert!(cut.ends_with("[truncated]"));
    }

    #[tokio::test]
    async fn disabled_never_sends() {
        // No token/channel: early return, nothing spawned.
        Alerter::disabled().send_text("hi".to_string());
        Alerter::new().send_text("hi".to_string());
        Alerter::new()
            .with_token("test-token-fixture")
            .send_text("hi".to_string());
        Alerter::new().send_card(Card::from("hi".to_string()));
    }

    #[tokio::test]
    async fn delivery_paths_do_not_panic() {
        // Connection-refused loopback: the error path only logs.
        // (No external network: nothing listens on port 9.)
        let alert = Alerter::with_api_base("http://127.0.0.1:9")
            .with_token("test-token-fixture")
            .with_channel("123456789");
        alert.send_text("hi".to_string());
        alert.send_card(Alerter::format_halt(&PAPER, 1.0, -1.0));
        // Let detached tasks run; assertions are "no panic, returns fast".
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    #[test]
    fn halt_card_and_breaker_line() {
        let card = Alerter::format_halt(&PAPER, 10.0, -12.34);
        assert!(card.content.contains("[DAY-LOSS HALT]"));
        assert!(card.content.contains("`paper · adaptive`"));
        assert!(card.content.contains("-$10.00/day"));
        assert!(card.content.contains("$-12.34"));
        assert!(card.content.contains("exits always work"));
        assert_eq!(Alerter::breaker_line(false, 0.0), "`armed`");
        assert_eq!(
            Alerter::breaker_line(true, -12.34),
            "`HALTED (day $-12.34)`"
        );
    }
}

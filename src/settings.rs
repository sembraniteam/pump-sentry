//! Runtime knobs for the paper pipeline, all overridable via env.
//!
//! [`Settings::from_env`] reads the process environment;
//! [`Settings::from_map`] takes an explicit map so unit tests never touch
//! global env state.

use risk_engine::StrategyMode;
use std::collections::HashMap;

/// Runtime knobs, all overridable via env.
///
/// Custom [`std::fmt::Debug`] redacts `inference_api_key`: a bearer token
/// must never appear in logs, even if a future debug-print of the whole
/// struct slips into an operational log line.
#[derive(Clone)]
pub struct Settings {
    /// WebSocket RPC URL for `logsSubscribe`.
    pub ws_url: String,
    /// HTTP RPC URL for `getTransaction` and reserve reads.
    pub http_url: String,
    /// Commitment: `processed` | `confirmed` | `finalized`.
    pub commitment: String,
    /// Virtual starting balance in USD.
    pub start_balance: f64,
    /// Commission per side in basis points.
    pub fee_bps: f64,
    /// Paper notional per trade in USD.
    pub notional_usd: f64,
    /// Trailing-stop giveback in percent.
    pub trail_percent: f64,
    /// Runner trailing-stop giveback in percent (default 20): moonbag
    /// runners shake out on normal 5% pullbacks, so the runner trail is
    /// wide enough to survive volatility on the way to 5x–10x. Used only
    /// in `runner` mode; must be > 0 to ever fire.
    pub runner_trail_percent: f64,
    /// Liquidity fallback in USD (used only when the reserve read fails).
    pub liquidity_usd: f64,
    /// Entry-gate window in seconds: buyer quorum must arrive in time.
    pub gate_window_secs: u64,
    /// Distinct non-dev buyers required to confirm an entry.
    pub gate_min_buyers: u32,
    /// Gate poll interval in milliseconds.
    pub gate_poll_ms: u64,
    /// Maximum concurrent paper positions (portfolio heat cap).
    pub max_open_positions: usize,
    /// Maximum realized day-loss in USD before new entries halt (`0` =
    /// disabled). UTC-day bucket; exits always keep working. Paper and
    /// live entries share the gate (live only mirrors paper opens).
    pub max_daily_loss_usd: f64,
    /// Per-mint cooldown in seconds after a close (`0` disables).
    pub mint_cooldown_secs: u64,
    /// Minimum dev-wallet age in seconds for the history screen.
    pub dev_min_age_secs: u64,
    /// Maximum tolerated prior Pump.fun launches by the dev.
    pub dev_max_prior_launches: usize,
    /// Recent dev transactions sampled for the launch count.
    pub dev_screen_sample_txs: usize,
    /// Manual entry-quote override for tests.
    pub quote_usd: Option<f64>,
    /// SOL→USD conversion for curve pricing (retired: feed is dynamic).
    /// Kept parsed so stale `SOL_USD=...` env lines warn instead of
    /// silently doing nothing; value is unused.
    pub _sol_usd_retired: f64,
    /// Bonding-curve poll interval in milliseconds.
    pub tick_interval_ms: u64,
    /// Post-migration pool poll interval in milliseconds (separate from
    /// the curve ticker: pool ticks only feed spike alerts, so they can
    /// run slower and save RPC budget).
    pub vault_tick_interval_ms: u64,
    /// OpenAI-compatible chat-completions endpoint.
    pub inference_endpoint: String,
    /// Model identifier for inference requests.
    pub inference_model: String,
    /// Inference deadline in milliseconds before heuristic fallback.
    pub inference_timeout_ms: u64,
    /// Optional bearer token for providers that require auth.
    pub inference_api_key: Option<String>,
    /// Buy/sell slippage guard in basis points.
    pub slippage_bps: u64,
    /// Execution strategy: `adaptive` (follow the risk profile), `scalp`
    /// (TP +25–35%, full exits), or `runner` (TP sells most, rides a
    /// trailing moonbag). Unknown values fall back to `adaptive` with a
    /// startup warning.
    pub strategy_mode: String,
    /// Runner share kept riding after TP, 0–100 (default 20 = sell 80%).
    /// Used only in `runner` mode; clamped into range at resolve time.
    pub runner_moonbag_percent: f64,
    /// Compute-unit price in micro-lamports for live transactions (`0` =
    /// no priority fee; standard `SetComputeUnitPrice` instruction when
    /// set). Raises landing odds under congestion at a per-tx cost.
    pub priority_fee_micro_lamports: u64,
    /// Compute-unit limit for live transactions (`0` = no limit
    /// instruction; the runtime default applies). Set below the default
    /// 1.4M only with a simulated margin — too low fails the tx.
    pub compute_unit_limit: u32,
    /// Jito tip budget per bundle in lamports.
    pub tip_lamports: u64,
    /// Jito block-engine base URL (region).
    pub jito_endpoint: String,
    /// Discord trade alerts via Hermes CLI (needs `hermes` on PATH).
    pub discord_alerts: bool,
    /// Optional Hermes Discord channel ID (numeric snowflake). Empty /
    /// unset preserves the skill's exact `send --to discord` argv, leaving
    /// channel routing to the host Hermes configuration. Set to address
    /// `--to discord:<channel-id>` for a specific channel.
    pub discord_channel: Option<String>,
    /// Strict preflight for live dispatch (`SIM_STRICT=true`): simulation
    /// transport/timeout failures reject the bundle instead of proceeding
    /// unproven. Default off (unknown sim results preserve today's
    /// warn-and-proceed); enable for live funds.
    pub sim_strict: bool,
    /// Parse failures seen by [`Settings::from_map`] (raw value → default
    /// used). Surfaced first by [`Settings::warnings`] so env typos never
    /// hide silently behind defaults.
    pub parse_notes: Vec<String>,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("ws_url", &self.ws_url)
            .field("http_url", &self.http_url)
            .field("commitment", &self.commitment)
            .field("start_balance", &self.start_balance)
            .field("fee_bps", &self.fee_bps)
            .field("notional_usd", &self.notional_usd)
            .field("trail_percent", &self.trail_percent)
            .field("runner_trail_percent", &self.runner_trail_percent)
            .field("liquidity_usd", &self.liquidity_usd)
            .field("gate_window_secs", &self.gate_window_secs)
            .field("gate_min_buyers", &self.gate_min_buyers)
            .field("gate_poll_ms", &self.gate_poll_ms)
            .field("max_open_positions", &self.max_open_positions)
            .field("max_daily_loss_usd", &self.max_daily_loss_usd)
            .field("mint_cooldown_secs", &self.mint_cooldown_secs)
            .field("dev_min_age_secs", &self.dev_min_age_secs)
            .field("dev_max_prior_launches", &self.dev_max_prior_launches)
            .field("dev_screen_sample_txs", &self.dev_screen_sample_txs)
            .field("quote_usd", &self.quote_usd)
            .field("sol_usd", &self._sol_usd_retired)
            .field("tick_interval_ms", &self.tick_interval_ms)
            .field("vault_tick_interval_ms", &self.vault_tick_interval_ms)
            .field("inference_endpoint", &self.inference_endpoint)
            .field("inference_model", &self.inference_model)
            .field("inference_timeout_ms", &self.inference_timeout_ms)
            .field(
                "inference_api_key",
                &self.inference_api_key.as_ref().map(|_| "<redacted>"),
            )
            .field("slippage_bps", &self.slippage_bps)
            .field("strategy_mode", &self.strategy_mode)
            .field("runner_moonbag_percent", &self.runner_moonbag_percent)
            .field("priority_fee_micro_lamports", &self.priority_fee_micro_lamports)
            .field("compute_unit_limit", &self.compute_unit_limit)
            .field("tip_lamports", &self.tip_lamports)
            .field("jito_endpoint", &self.jito_endpoint)
            .field("discord_alerts", &self.discord_alerts)
            .field("discord_channel", &self.discord_channel)
            .field("sim_strict", &self.sim_strict)
            .field("parse_notes", &self.parse_notes)
            .finish()
    }
}

impl Settings {
    /// Read knobs from the process environment.
    pub fn from_env() -> Self {
        Self::from_map(&std::env::vars().collect())
    }

    /// Build knobs from an explicit map (test-friendly, no global state).
    pub fn from_map(vars: &HashMap<String, String>) -> Self {        let mut notes = Vec::new();
        Self {
            ws_url: parse_wss_url(vars, &mut notes),
            http_url: parse_rpc_url(vars, &mut notes),
            commitment: get_string(vars, "COMMITMENT", "confirmed"),
            start_balance: get_float(vars, "START_BALANCE", 100.0, &mut notes),
            fee_bps: get_float(vars, "FEE_BPS", 30.0, &mut notes),
            notional_usd: get_float(vars, "NOTIONAL_USD", 10.0, &mut notes),
            trail_percent: get_float(vars, "TRAIL_PERCENT", 5.0, &mut notes),
            runner_trail_percent: get_float(vars, "RUNNER_TRAIL_PERCENT", 20.0, &mut notes),
            liquidity_usd: get_float(vars, "LIQUIDITY_USD", 0.0, &mut notes),
            gate_window_secs: get_int(vars, "GATE_WINDOW_SECS", 10, &mut notes),
            gate_min_buyers: get_int(vars, "GATE_MIN_BUYERS", 5, &mut notes),
            gate_poll_ms: get_int(vars, "GATE_POLL_MS", 2000, &mut notes),
            max_open_positions: get_int(vars, "MAX_OPEN_POSITIONS", 3, &mut notes),
            max_daily_loss_usd: get_float(vars, "MAX_DAILY_LOSS_USD", 0.0, &mut notes),
            mint_cooldown_secs: get_int(vars, "MINT_COOLDOWN_SECS", 3600, &mut notes),
            dev_min_age_secs: get_int(vars, "DEV_MIN_AGE_SECS", 3600, &mut notes),
            dev_max_prior_launches: get_int(vars, "DEV_MAX_PRIOR_LAUNCHES", 1, &mut notes),
            dev_screen_sample_txs: get_int(vars, "DEV_SCREEN_SAMPLE_TXS", 20, &mut notes),
            quote_usd: parse_quote_usd(vars, &mut notes),
            _sol_usd_retired: parse_retired_sol_usd(vars, &mut notes),
            tick_interval_ms: get_int(vars, "TICK_INTERVAL_MS", 1000, &mut notes),
            vault_tick_interval_ms: get_int(vars, "VAULT_TICK_INTERVAL_MS", 5000, &mut notes),
            inference_endpoint: get_string(
                vars,
                "INFERENCE_ENDPOINT",
                "http://127.0.0.1:11434/v1/chat/completions",
            ),
            inference_model: get_string(vars, "INFERENCE_MODEL", "llama3.2:1b"),
            inference_timeout_ms: get_int(vars, "INFERENCE_TIMEOUT_MS", 300, &mut notes),
            inference_api_key: vars.get("INFERENCE_API_KEY").cloned(),
            slippage_bps: get_int(vars, "SLIPPAGE_BPS", 200, &mut notes),
            strategy_mode: get_string(vars, "STRATEGY_MODE", "adaptive"),
            runner_moonbag_percent: get_float(vars, "RUNNER_MOONBAG_PERCENT", 20.0, &mut notes),
            priority_fee_micro_lamports: get_int(vars, "PRIORITY_FEE_MICRO_LAMPORTS", 0, &mut notes),
            compute_unit_limit: get_int(vars, "COMPUTE_UNIT_LIMIT", 0, &mut notes),
            tip_lamports: get_int(vars, "JITO_TIP_LAMPORTS", 100_000, &mut notes),
            jito_endpoint: get_string(
                vars,
                "JITO_ENDPOINT",
                "https://frankfurt.mainnet.block-engine.jito.wtf",
            ),
            discord_alerts: get_bool(vars, "DISCORD_ALERTS", true, &mut notes),
            discord_channel: vars
                .get("DISCORD_CHANNEL")
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            sim_strict: get_bool(vars, "SIM_STRICT", false, &mut notes),
            parse_notes: notes,
        }
    }

    /// Human-readable warnings for suspicious values. Parse failures are
    /// recorded by `from_map` (see `parse_notes`) and surface first, so
    /// typos never hide silently behind defaults; `main` prints these at
    /// startup and refuses zero poll intervals outright. Empty when
    /// everything looks sane.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = self.parse_notes.clone();
        if self.slippage_bps > 10_000 {
            out.push(format!(
                "SLIPPAGE_BPS={} exceeds 100%; clamped to 100% at sizing (check the value — likely a typo)",
                self.slippage_bps
            ));
        }
        if self.strategy_mode.parse::<StrategyMode>().is_err() {
            out.push(format!(
                "STRATEGY_MODE='{}' unknown; falling back to adaptive",
                self.strategy_mode
            ));
        }
        if !self.runner_moonbag_percent.is_finite()
            || self.runner_moonbag_percent < 0.0
            || self.runner_moonbag_percent > 100.0
        {
            out.push(format!(
                "RUNNER_MOONBAG_PERCENT={} outside 0–100; clamped at resolve time",
                self.runner_moonbag_percent
            ));
        }
        if !self.runner_trail_percent.is_finite() || self.runner_trail_percent <= 0.0 {
            out.push(format!(
                "RUNNER_TRAIL_PERCENT={} must be positive; runners would never trail-exit",
                self.runner_trail_percent
            ));
        }
        if self.notional_usd <= 0.0 || !self.notional_usd.is_finite() {
            out.push(format!(
                "NOTIONAL_USD={} is not positive; no position can open",
                self.notional_usd
            ));
        }
        if self.fee_bps < 0.0 || !self.fee_bps.is_finite() {
            out.push(format!("FEE_BPS={} is invalid", self.fee_bps));
        }
        if self.max_open_positions == 0 {
            out.push("MAX_OPEN_POSITIONS=0; the pipeline can never open".to_string());
        }
        if self.max_daily_loss_usd < 0.0 || !self.max_daily_loss_usd.is_finite() {
            out.push(format!(
                "MAX_DAILY_LOSS_USD={} is invalid; breaker disabled",
                self.max_daily_loss_usd
            ));
        }
        if self.gate_min_buyers == 0 {
            out.push("GATE_MIN_BUYERS=0 behaves as 1 (quorum floor)".to_string());
        }
        if self._sol_usd_retired > 0.0 && self._sol_usd_retired.is_finite() {
            out.push(format!(
                "SOL_USD={} is retired: pricing now uses a live Jupiter feed (dynamic, stale-gated); this env value is ignored",
                self._sol_usd_retired
            ));
        }
        if self.commitment == "processed" {
            out.push(
                "COMMITMENT=processed is optimistic (reorgable prints): prefer confirmed for exits"
                    .to_string(),
            );
        }
        if self.start_balance <= 0.0 || !self.start_balance.is_finite() {
            out.push(format!(
                "START_BALANCE={} is not positive finite; the paper book starts broke",
                self.start_balance
            ));
        }
        if !self.trail_percent.is_finite() || self.trail_percent <= 0.0 {
            out.push(format!(
                "TRAIL_PERCENT={} must be positive finite; trailing exits would never fire",
                self.trail_percent
            ));
        }
        if !self.liquidity_usd.is_finite() || self.liquidity_usd < 0.0 {
            out.push(format!(
                "LIQUIDITY_USD={} is invalid; the reserve-failure fallback is garbage",
                self.liquidity_usd
            ));
        }
        if self.gate_poll_ms == 0 {
            out.push("GATE_POLL_MS=0 must be positive; startup refuses this".to_string());
        }
        if self.tick_interval_ms == 0 {
            out.push("TICK_INTERVAL_MS=0 must be positive; startup refuses this".to_string());
        }
        if self.vault_tick_interval_ms == 0 {
            out.push("VAULT_TICK_INTERVAL_MS=0 must be positive; startup refuses this".to_string());
        }
        if self.inference_timeout_ms == 0 {
            out.push(
                "INFERENCE_TIMEOUT_MS=0; every inference falls back to the heuristic".to_string(),
            );
        }
        if self.tip_lamports == 0 {
            out.push("JITO_TIP_LAMPORTS=0; bundles carry no tip and may never land".to_string());
        }
        if self.quote_usd.is_some() {
            out.push(
                "QUOTE_USD override is set: entries fall back to a manual price when the feed fails (paper-only by construction)".to_string(),
            );
        }
        out
    }
}

fn get_string(vars: &HashMap<String, String>, key: &str, default: &str) -> String {
    vars.get(key)
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

fn get_float(
    vars: &HashMap<String, String>,
    key: &str,
    default: f64,
    notes: &mut Vec<String>,
) -> f64 {
    match vars.get(key) {
        None => default,
        Some(v) => match v.parse() {
            Ok(x) => x,
            Err(_) => {
                notes.push(format!(
                    "{key}='{v}' is not a number; using default {default}"
                ));
                default
            }
        },
    }
}

fn get_int<T: std::str::FromStr + std::fmt::Display>(
    vars: &HashMap<String, String>,
    key: &str,
    default: T,
    notes: &mut Vec<String>,
) -> T {
    match vars.get(key) {
        None => default,
        Some(v) => match v.parse() {
            Ok(x) => x,
            Err(_) => {
                notes.push(format!(
                    "{key}='{v}' is not an integer; using default {default}"
                ));
                default
            }
        },
    }
}

fn get_bool(
    vars: &HashMap<String, String>,
    key: &str,
    default: bool,
    notes: &mut Vec<String>,
) -> bool {
    match vars.get(key).map(|v| v.to_ascii_lowercase()) {
        None => default,
        Some(v) if matches!(v.as_str(), "true" | "1" | "yes") => true,
        Some(v) if matches!(v.as_str(), "false" | "0" | "no" | "") => false,
        Some(v) => {
            notes.push(format!("{key}='{v}' unrecognized; using default {default}"));
            default
        }
    }
}

/// Retired static price: returns a positive finite sentinel or 0 when
/// unset, with a note when a stale env line is ignored.
fn parse_retired_sol_usd(vars: &HashMap<String, String>, notes: &mut Vec<String>) -> f64 {
    match vars.get("SOL_USD") {
        None => 0.0,
        Some(raw) => match raw.parse::<f64>() {
            Ok(v) if v.is_finite() && v > 0.0 => {
                notes.push(format!(
                    "SOL_USD={v} is retired: pricing uses the live Jupiter feed; ignoring"
                ));
                v
            }
            Ok(_) => {
                notes.push("SOL_USD is retired and not a positive price; ignoring".to_string());
                0.0
            }
            Err(_) => {
                notes.push(format!("SOL_USD='{raw}' is not a number; ignoring"));
                0.0
            }
        },
    }
}

/// Canonical WebSocket RPC URL: `MAINNET_WSS_RPC_URL` is the only
/// configured name. A legacy `WS_URL` is still honored once with a loud
/// retirement note (same grace as `HTTP_URL` → `MAINNET_RPC_URL`).
/// Unset = public mainnet.
fn parse_wss_url(vars: &HashMap<String, String>, notes: &mut Vec<String>) -> String {
    const DEFAULT: &str = "wss://api.mainnet-beta.solana.com";
    let clean = |v: &String| v.replace('\\', "").trim().to_string();
    match vars.get("MAINNET_WSS_RPC_URL").map(clean).filter(|v| !v.is_empty()) {
        Some(url) => url,
        None => match vars.get("WS_URL").map(clean).filter(|v| !v.is_empty()) {
            Some(legacy) => {
                notes.push(
                    "WS_URL is retired: rename it to MAINNET_WSS_RPC_URL (honored this run)".to_string(),
                );
                legacy
            }
            None => DEFAULT.to_string(),
        },
    }
}

/// Canonical HTTP RPC URL: `MAINNET_RPC_URL` is the only configured
/// name. A legacy `HTTP_URL` is still honored once with a loud
/// retirement note (so a running deployment is never silently dropped
/// to the rate-limited public endpoint); backslash escapes are stripped
/// from either (Helius lines arrive escaped through some shells).
/// Unset = public mainnet.
fn parse_rpc_url(vars: &HashMap<String, String>, notes: &mut Vec<String>) -> String {
    const DEFAULT: &str = "https://api.mainnet-beta.solana.com";
    let clean = |v: &String| v.replace('\\', "").trim().to_string();
    match vars.get("MAINNET_RPC_URL").map(clean).filter(|v| !v.is_empty()) {
        Some(url) => url,
        None => match vars.get("HTTP_URL").map(clean).filter(|v| !v.is_empty()) {
            Some(legacy) => {
                notes.push(
                    "HTTP_URL is retired: rename it to MAINNET_RPC_URL (honored this run)".to_string(),
                );
                legacy
            }
            None => DEFAULT.to_string(),
        },
    }
}

/// Manual entry-quote override: only a positive finite price is honored.
/// Anything else is ignored with a note (a NaN/negative override would
/// otherwise price paper opens — and live mirrors — at garbage).
fn parse_quote_usd(vars: &HashMap<String, String>, notes: &mut Vec<String>) -> Option<f64> {
    let raw = vars.get("QUOTE_USD")?;
    match raw.parse::<f64>() {
        Ok(q) if q.is_finite() && q > 0.0 => Some(q),
        Ok(q) => {
            notes.push(format!(
                "QUOTE_USD={q} is not a positive finite price; ignoring the override"
            ));
            None
        }
        Err(_) => {
            notes.push(format!(
                "QUOTE_USD='{raw}' is not a number; ignoring the override"
            ));
            None
        }
    }
}

/// Local unix seconds; `0` when the clock is unavailable (never panics).
/// A broken clock corrupts day buckets, gate deadlines, and cooldowns —
/// warn once (loud on first use, silent after, so a wedged clock cannot
/// spam every tick). `saturating_sub` everywhere keeps clock-0 (and any
/// backward step) fail-open, never panicking or underflowing — at worst
/// timeouts fire immediately and cooldowns look expired.
pub fn now_secs() -> u64 {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static WARNED: AtomicBool = AtomicBool::new(false);
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs(),
        Err(_) => {
            if !WARNED.swap(true, Ordering::Relaxed) {
                tracing::warn!("system clock before unix epoch; time math degrades to 0");
            }
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn defaults_are_sane() {
        let s = Settings::from_map(&HashMap::new());
        assert_eq!(s.commitment, "confirmed");
        assert_eq!(s.start_balance, 100.0);
        assert_eq!(s.gate_window_secs, 10);
        assert_eq!(s.gate_min_buyers, 5);
        assert_eq!(s.max_open_positions, 3);
        assert_eq!(s.mint_cooldown_secs, 3600);
        assert_eq!(s.dev_min_age_secs, 3600);
        assert_eq!(s.quote_usd, None);
        assert_eq!(s.inference_timeout_ms, 300);
        assert_eq!(s.inference_api_key, None);
        assert_eq!(s.strategy_mode, "adaptive");
        assert_eq!(s.runner_moonbag_percent, 20.0);
        assert_eq!(s.runner_trail_percent, 20.0);
        assert!(!s.sim_strict);
        assert!(s.parse_notes.is_empty());
    }

    #[test]
    fn rpc_urls_prefer_mainnet_vars_strip_escapes_and_retire_legacy() {
        // Canonical var wins.
        let s = Settings::from_map(&vars(&[
            ("MAINNET_RPC_URL", "https://helius/abc"),
            ("HTTP_URL", "https://legacy/xyz"),
        ]));
        assert_eq!(s.http_url, "https://helius/abc");
        assert!(s.parse_notes.is_empty());
        // Backslash escapes stripped.
        let s = Settings::from_map(&vars(&[("MAINNET_RPC_URL", "https:\\\\/\\\\/helius")]));
        assert_eq!(s.http_url, "https://helius");
        // Legacy honored once, loudly.
        let s = Settings::from_map(&vars(&[("HTTP_URL", "https://legacy/xyz")]));
        assert_eq!(s.http_url, "https://legacy/xyz");
        assert!(s.warnings().iter().any(|w| w.contains("HTTP_URL is retired")), "{:?}", s.warnings());
        // Unset = public default.
        let s = Settings::from_map(&HashMap::new());
        assert_eq!(s.http_url, "https://api.mainnet-beta.solana.com");
        assert_eq!(s.ws_url, "wss://api.mainnet-beta.solana.com");
        // WSS mirrors the HTTP rule: canonical wins, legacy warns.
        let s = Settings::from_map(&vars(&[
            ("MAINNET_WSS_RPC_URL", "wss://helius/abc"),
            ("WS_URL", "wss://legacy/xyz"),
        ]));
        assert_eq!(s.ws_url, "wss://helius/abc");
        let s = Settings::from_map(&vars(&[("WS_URL", "wss://legacy/xyz")]));
        assert_eq!(s.ws_url, "wss://legacy/xyz");
        assert!(s.warnings().iter().any(|w| w.contains("WS_URL is retired")), "{:?}", s.warnings());
    }

    #[test]
    fn overrides_apply() {
        let s = Settings::from_map(&vars(&[
            ("START_BALANCE", "250.5"),
            ("GATE_MIN_BUYERS", "5"),
            ("QUOTE_USD", "0.000001"),
            ("COMMITMENT", "finalized"),
            ("SLIPPAGE_BPS", "500"),
            ("JITO_TIP_LAMPORTS", "200000"),
            ("INFERENCE_API_KEY", "secret"),
            ("SIM_STRICT", "true"),
        ]));
        assert_eq!(s.start_balance, 250.5);
        assert_eq!(s.gate_min_buyers, 5);
        assert_eq!(s.quote_usd, Some(0.000001));
        assert_eq!(s.commitment, "finalized");
        assert_eq!(s.slippage_bps, 500);
        assert_eq!(s.tip_lamports, 200_000);
        assert_eq!(s.inference_api_key.as_deref(), Some("secret"));
        assert!(s.sim_strict);
    }

    #[test]
    fn invalid_values_fall_back() {
        let s = Settings::from_map(&vars(&[
            ("START_BALANCE", "bogus"),
            ("TICK_INTERVAL_MS", "-3"),
            ("QUOTE_USD", "nan-ish"),
        ]));
        assert_eq!(s.start_balance, 100.0);
        assert_eq!(s.tick_interval_ms, 1000);
        assert_eq!(s.quote_usd, None);
    }

    #[test]
    fn invalid_values_warn_loudly() {
        // Formerly silent fallbacks: every typo must surface in warnings.
        let s = Settings::from_map(&vars(&[
            ("START_BALANCE", "bogus"),
            ("GATE_POLL_MS", "zero"),
            ("QUOTE_USD", "-5"),
            ("DISCORD_ALERTS", "perhaps"),
        ]));
        let w = s.warnings().join("\n");
        assert!(w.contains("START_BALANCE='bogus'"), "{w}");
        assert!(w.contains("GATE_POLL_MS='zero'"), "{w}");
        assert!(w.contains("QUOTE_USD=-5"), "{w}");
        assert!(w.contains("DISCORD_ALERTS='perhaps'"), "{w}");
    }

    #[test]
    fn degenerate_values_warn() {        let s = Settings::from_map(&vars(&[
            ("GATE_POLL_MS", "0"),
            ("TICK_INTERVAL_MS", "0"),
            ("TRAIL_PERCENT", "0"),
            ("INFERENCE_TIMEOUT_MS", "0"),
            ("JITO_TIP_LAMPORTS", "0"),
            ("SLIPPAGE_BPS", "20000"),
            ("QUOTE_USD", "0.000001"),
        ]));
        let w = s.warnings().join("\n");
        assert!(w.contains("GATE_POLL_MS=0"), "{w}");
        assert!(w.contains("TICK_INTERVAL_MS=0"), "{w}");
        assert!(w.contains("TRAIL_PERCENT=0"), "{w}");
        assert!(w.contains("INFERENCE_TIMEOUT_MS=0"), "{w}");
        assert!(w.contains("JITO_TIP_LAMPORTS=0"), "{w}");
        assert!(w.contains("SLIPPAGE_BPS=20000"), "{w}");
        assert!(w.contains("QUOTE_USD override is set"), "{w}");
    }

    #[test]
    fn processed_commitment_warns() {
        let s = Settings::from_map(&vars(&[("COMMITMENT", "processed")]));
        let w = s.warnings().join("\n");
        assert!(w.contains("COMMITMENT=processed"), "{w}");
        let s = Settings::from_map(&HashMap::new());
        assert!(!s.warnings().iter().any(|x| x.contains("COMMITMENT=")));
    }

    #[test]
    fn debug_redacts_api_key() {
        // A bearer token must survive even a careless whole-struct
        // debug-print into an ops log.
        let s = Settings::from_map(&vars(&[("INFERENCE_API_KEY", "super-secret")]));
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("super-secret"), "{dbg}");
        assert!(dbg.contains("<redacted>"), "{dbg}");
        let clean = format!("{:?}", Settings::from_map(&HashMap::new()));
        assert!(!clean.contains("super-secret"), "{clean}");
    }

    #[test]
    fn now_secs_is_sane() {
        assert!(now_secs() > 1_700_000_000);
    }
}

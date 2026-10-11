//! Runtime knobs for the paper pipeline, all overridable via env.
//!
//! [`Settings::from_env`] reads the process environment;
//! [`Settings::from_map`] takes an explicit map so unit tests never touch
//! global env state.

use risk_engine::StrategyMode;
use std::collections::HashMap;

/// Runtime knobs, all overridable via env.
#[derive(Debug, Clone)]
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
    /// SOL→USD conversion for curve pricing.
    pub sol_usd: f64,
    /// Bonding-curve poll interval in milliseconds.
    pub tick_interval_ms: u64,
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
    /// Override fee-recipient account for swap instructions (base58).
    /// `None` uses the canonical recipient. Set when the program rotates
    /// recipients; the IDL takes it as a caller-supplied account.
    pub fee_recipient: Option<String>,
    /// Jito block-engine base URL (region).
    pub jito_endpoint: String,
    /// Discord trade alerts via Hermes CLI (needs `hermes` on PATH).
    pub discord_alerts: bool,
    /// Optional Hermes Discord channel ID (numeric snowflake). Empty /
    /// unset preserves the skill's exact `send --to discord` argv, leaving
    /// channel routing to the host Hermes configuration. Set to address
    /// `--to discord:<channel-id>` for a specific channel.
    pub discord_channel: Option<String>,
}

impl Settings {
    /// Read knobs from the process environment.
    pub fn from_env() -> Self {
        Self::from_map(&std::env::vars().collect())
    }

    /// Build knobs from an explicit map (test-friendly, no global state).
    pub fn from_map(vars: &HashMap<String, String>) -> Self {        Self {
            ws_url: get_string(vars, "WS_URL", "wss://api.mainnet-beta.solana.com"),
            http_url: get_string(vars, "HTTP_URL", "https://api.mainnet-beta.solana.com"),
            commitment: get_string(vars, "COMMITMENT", "confirmed"),
            start_balance: get_float(vars, "START_BALANCE", 100.0),
            fee_bps: get_float(vars, "FEE_BPS", 30.0),
            notional_usd: get_float(vars, "NOTIONAL_USD", 10.0),
            trail_percent: get_float(vars, "TRAIL_PERCENT", 5.0),
            runner_trail_percent: get_float(vars, "RUNNER_TRAIL_PERCENT", 20.0),
            liquidity_usd: get_float(vars, "LIQUIDITY_USD", 0.0),
            gate_window_secs: get_int(vars, "GATE_WINDOW_SECS", 10),
            gate_min_buyers: get_int(vars, "GATE_MIN_BUYERS", 5),
            gate_poll_ms: get_int(vars, "GATE_POLL_MS", 2000),
            max_open_positions: get_int(vars, "MAX_OPEN_POSITIONS", 3),
            max_daily_loss_usd: get_float(vars, "MAX_DAILY_LOSS_USD", 0.0),
            mint_cooldown_secs: get_int(vars, "MINT_COOLDOWN_SECS", 3600),
            dev_min_age_secs: get_int(vars, "DEV_MIN_AGE_SECS", 3600),
            dev_max_prior_launches: get_int(vars, "DEV_MAX_PRIOR_LAUNCHES", 1),
            dev_screen_sample_txs: get_int(vars, "DEV_SCREEN_SAMPLE_TXS", 20),
            quote_usd: vars.get("QUOTE_USD").and_then(|v| v.parse().ok()),
            sol_usd: get_float(vars, "SOL_USD", 150.0),
            tick_interval_ms: get_int(vars, "TICK_INTERVAL_MS", 1000),
            inference_endpoint: get_string(
                vars,
                "INFERENCE_ENDPOINT",
                "http://127.0.0.1:11434/v1/chat/completions",
            ),
            inference_model: get_string(vars, "INFERENCE_MODEL", "llama3.2:1b"),
            inference_timeout_ms: get_int(vars, "INFERENCE_TIMEOUT_MS", 300),
            inference_api_key: vars.get("INFERENCE_API_KEY").cloned(),
            slippage_bps: get_int(vars, "SLIPPAGE_BPS", 200),
            strategy_mode: get_string(vars, "STRATEGY_MODE", "adaptive"),
            runner_moonbag_percent: get_float(vars, "RUNNER_MOONBAG_PERCENT", 20.0),
            priority_fee_micro_lamports: get_int(vars, "PRIORITY_FEE_MICRO_LAMPORTS", 0),
            compute_unit_limit: get_int(vars, "COMPUTE_UNIT_LIMIT", 0),
            tip_lamports: get_int(vars, "JITO_TIP_LAMPORTS", 100_000),
            fee_recipient: vars
                .get("FEE_RECIPIENT")
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            jito_endpoint: get_string(
                vars,
                "JITO_ENDPOINT",
                "https://frankfurt.mainnet.block-engine.jito.wtf",
            ),
            discord_alerts: get_bool(vars, "DISCORD_ALERTS", true),
            discord_channel: vars
                .get("DISCORD_CHANNEL")
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
        }
    }

    /// Human-readable warnings for suspicious values (typos fall back to
    /// defaults silently in `from_map`, so surface them here; `main`
    /// prints these at startup). Empty when everything looks sane.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.slippage_bps > 10_000 {
            out.push(format!(
                "SLIPPAGE_BPS={} exceeds 100%; buy guard will overspend massively",
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
        if self.sol_usd <= 0.0 || !self.sol_usd.is_finite() {
            out.push(format!("SOL_USD={} is not positive; pricing is garbage", self.sol_usd));
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

fn get_float(vars: &HashMap<String, String>, key: &str, default: f64) -> f64 {
    vars.get(key)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn get_int<T: std::str::FromStr>(vars: &HashMap<String, String>, key: &str, default: T) -> T {
    vars.get(key)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn get_bool(vars: &HashMap<String, String>, key: &str, default: bool) -> bool {
    vars.get(key)
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
        .unwrap_or(default)
}

/// Local unix seconds; `0` when the clock is unavailable (never panics).
pub fn now_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
        ]));
        assert_eq!(s.start_balance, 250.5);
        assert_eq!(s.gate_min_buyers, 5);
        assert_eq!(s.quote_usd, Some(0.000001));
        assert_eq!(s.commitment, "finalized");
        assert_eq!(s.slippage_bps, 500);
        assert_eq!(s.tip_lamports, 200_000);
        assert_eq!(s.inference_api_key.as_deref(), Some("secret"));
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
    fn now_secs_is_sane() {
        assert!(now_secs() > 1_700_000_000);
    }
}

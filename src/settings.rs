//! Runtime knobs, all tunable via config.toml (secrets stay in env).

use risk_engine::{ExitRegime, StrategyMode};
use std::collections::HashMap;
use zeroize::Zeroizing;

/// Default Jito block-engine base URL (override via `JITO_ENDPOINT`).
/// Global endpoint stays the default (anycast reachability over shaving
/// auction latency); point at the nearest region when it matters.
pub const DEFAULT_JITO_ENDPOINT: &str = "https://mainnet.block-engine.jito.wtf";
/// Default local chat-completions endpoint (override via `INFERENCE_ENDPOINT`).
pub const DEFAULT_INFERENCE_ENDPOINT: &str = "http://127.0.0.1:11434/v1/chat/completions";

/// Runtime knobs, all tunable via config.toml (secrets stay in env).
#[derive(Clone)]
pub struct Settings {
    pub ws_url: String,
    pub http_url: String,
    /// Read-path failover endpoints (first = primary `http_url`).
    /// Dispatch never fans out — these harden READS only.
    pub fallback_rpc_urls: Vec<String>,
    /// Yellowstone gRPC endpoint ("" = unset → WS ingestion).
    pub yellowstone_endpoint: String,
    /// `processed` | `confirmed` | `finalized`.
    pub commitment: String,
    /// Virtual balance in USD.
    pub start_balance: f64,
    /// Extra commission per side in bps (stacks on the venue fee).
    /// Future: migrate to `crate::money::Bps` (see money.rs TODO).
    pub fee_bps: f64,
    /// Paper notional per trade in USD.
    pub notional_usd: f64,
    /// Trailing giveback in percent.
    pub trail_percent: f64,
    /// Runner trail in percent (wide, for 5x–10x runs).
    pub runner_trail_percent: f64,
    /// Liquidity fallback in USD (reserve read fails only).
    pub liquidity_usd: f64,
    /// Entry-gate window in seconds.
    pub gate_window_secs: u64,
    /// Buyers needed to confirm.
    pub gate_min_buyers: u32,
    /// Gate poll interval in ms.
    pub gate_poll_ms: u64,
    /// Max paper positions.
    pub max_open_positions: usize,
    /// Day-loss cap in USD (`0` = off). Exits always work.
    pub max_daily_loss_usd: f64,
    /// Total open-exposure cap in USD (`0` = off): paper cost basis
    /// plus one ticket per unpaired live bag.
    pub max_exposure_usd: f64,
    /// Proven-bleed entry cap in USD per ticket (`0` = off): refuses an
    /// open when this ticket shape's settled entry-gap cell averages
    /// worse than this (live paid more than paper by more than the
    /// cap). Thin cells (unknown) always allow; exits never block.
    pub max_expected_entry_gap_usd: f64,
    /// Top-10 holder concentration cap in percent of supply (`0` = off):
    /// refuses an open when the ten largest holders own more than this.
    /// Unreadable holdings refuse while armed (fail-closed); exits never
    /// block.
    pub max_top_holder_pct: f64,
    /// Buy fill-rate floor in percent (`0` = off): the supervisor pauses
    /// entries while the trailing settled-outcome window fills below
    /// this. Thin windows read as unknown, never as trips; recovery is
    /// a passing re-check, never elapsed time.
    pub min_fill_rate_pct: f64,
    /// Paper-vs-live exit-gap trip in USD (`0` = off): the supervisor
    /// pauses entries while the retained-window average exit gap runs
    /// worse than this (negative = live did worse). Now-windowed
    /// averages only; recovery is a healthy re-check, never a timer.
    pub max_avg_exit_gap_usd: f64,
    /// Same as `max_avg_exit_gap_usd` for the entry leg (paper quote vs
    /// landed fill). `0` = off.
    pub max_avg_entry_gap_usd: f64,
    /// Hot-wallet ceiling in SOL (`0` = off): entries refuse while the
    /// wallet holds more than this. Exits always work.
    pub hot_wallet_max_sol: f64,
    /// Per-mint cooldown after close (`0` = off).
    pub mint_cooldown_secs: u64,
    /// Min dev-wallet age in seconds.
    pub dev_min_age_secs: u64,
    /// Max tolerated dev prior launches.
    pub dev_max_prior_launches: usize,
    /// Dev txs sampled for the launch count.
    pub dev_screen_sample_txs: usize,
    /// Dev signature window fetched per wallet.
    pub dev_sig_limit: usize,
    /// Manual entry quote (tests).
    pub quote_usd: Option<f64>,
    /// Curve poll interval in ms.
    pub tick_interval_ms: u64,
    /// Pool poll interval in ms.
    pub vault_tick_interval_ms: u64,
    /// Concurrent `getTransaction` fetches (listener semaphore).
    /// `0` refuses at boot (a zero semaphore blinds ingestion).
    pub fetch_concurrency: usize,
    /// Signatures per reconnect backfill (`0` = backfill off).
    pub backfill_sig_limit: usize,
    /// Reconnect backoff cap in ms (flaky sockets: raise to space out
    /// reconnect storms; floor 100ms keeps shutdown responsive).
    pub reconnect_backoff_max_ms: u64,
    /// Chat-completions endpoint.
    pub inference_endpoint: String,
    /// Model id for inference.
    pub inference_model: String,
    /// Inference deadline in ms.
    pub inference_timeout_ms: u64,
    /// Bearer token, if the provider needs one. Zeroized on drop;
    /// never logged (Debug redacts), never read from the file.
    pub inference_api_key: Option<Zeroizing<String>>,
    /// Optional extra headers sent on every inference request, as
    /// `"Name: value, ..."` (e.g. OpenCode Go `x-opencode-session`).
    /// Empty (default) sends none. `authorization`/`user-agent` and
    /// framing headers are refused at parse with a note (auth stays
    /// in `INFERENCE_API_KEY` env, the user agent is owned). Values
    /// never appear in logs.
    pub inference_headers: Vec<(String, String)>,
    /// Slippage guard in bps.
    pub slippage_bps: u64,
    /// Volatility window for adaptive slippage, secs (`0` = off).
    pub slippage_vol_window_secs: u64,
    /// Cap on the volatility slippage add-on, bps (`0` = off).
    pub slippage_vol_extra_cap_bps: u64,
    /// `adaptive` | `scalp` | `runner`. Unknown falls back.
    pub strategy_mode: String,
    /// Inference-failure policy: `heuristic` (validated bands), `pass`
    /// (skip the entry), or `defer` (rewatch, retry the model on a
    /// later confirm — engine-present failures only; exhausted defers
    /// land on heuristic). Never a hotter fallback to protect trade
    /// count; unknown values warn and stay heuristic.
    pub inference_fallback: String,
    /// Confirm-level defers allowed per signal before landing on the
    /// heuristic (policy `defer` only). `0` = off (heuristic at once);
    /// each defer burns one full confirm round of RPC budget, so the
    /// signal's own deadline stays the hard stop.
    pub inference_defer_max_n: u32,
    /// Forced exit regime (`""` = LLM selects from the closed set):
    /// fixed baseline for evaluation, and the LLM kill path — set it
    /// permanently if selection never beats the baseline. Unknown
    /// non-empty values warn at boot and select nothing (LLM decides).
    pub force_exit_regime: String,
    /// Runner share kept after TP, 0–100.
    pub runner_moonbag_percent: f64,
    /// Priority fee price in micro-lamports.
    pub priority_fee_micro_lamports: u64,
    /// Compute-unit limit (`0` = default).
    pub compute_unit_limit: u32,
    /// Jito tip per bundle in lamports.
    pub tip_lamports: u64,
    /// Jito block-engine URL.
    pub jito_endpoint: String,
    /// Discord alerts via Hermes CLI.
    pub discord_alerts: bool,
    /// Hermes channel id. Empty uses host config.
    pub discord_channel: Option<String>,
    /// Strict preflight: sim failures reject. Use for live.
    pub sim_strict: bool,
    /// Diverged-count warn threshold on the heartbeat (`0` = off).
    pub alert_diverged_warn_n: u64,
    /// Risk-engine fallback-rate warn threshold, pct (`0` = off).
    pub alert_risk_fallback_warn_pct: u64,
    /// GiveUp-burst warn threshold per window (`0` = off).
    pub alert_giveup_burst_n: u64,
    /// Trailing window for the GiveUp burst, secs.
    pub alert_giveup_window_secs: u64,
    /// Parse failures seen by `from_map`. Surfaced in `warnings`.
    pub parse_notes: Vec<String>,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("ws_url", &ids::redact_endpoint(&self.ws_url))
            .field("http_url", &ids::redact_endpoint(&self.http_url))
            .field(
                "fallback_rpc_urls",
                &self
                    .fallback_rpc_urls
                    .iter()
                    .map(|u| ids::redact_endpoint(u))
                    .collect::<Vec<_>>(),
            )
            .field("yellowstone_endpoint", &ids::redact_endpoint(&self.yellowstone_endpoint))
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
            .field("max_exposure_usd", &self.max_exposure_usd)
            .field(
                "max_expected_entry_gap_usd",
                &self.max_expected_entry_gap_usd,
            )
            .field("max_top_holder_pct", &self.max_top_holder_pct)
            .field("min_fill_rate_pct", &self.min_fill_rate_pct)
            .field("max_avg_exit_gap_usd", &self.max_avg_exit_gap_usd)
            .field("max_avg_entry_gap_usd", &self.max_avg_entry_gap_usd)
            .field("hot_wallet_max_sol", &self.hot_wallet_max_sol)
            .field("mint_cooldown_secs", &self.mint_cooldown_secs)
            .field("dev_min_age_secs", &self.dev_min_age_secs)
            .field("dev_max_prior_launches", &self.dev_max_prior_launches)
            .field("dev_screen_sample_txs", &self.dev_screen_sample_txs)
            .field("dev_sig_limit", &self.dev_sig_limit)
            .field("quote_usd", &self.quote_usd)
            .field("tick_interval_ms", &self.tick_interval_ms)
            .field("vault_tick_interval_ms", &self.vault_tick_interval_ms)
            .field("fetch_concurrency", &self.fetch_concurrency)
            .field("backfill_sig_limit", &self.backfill_sig_limit)
            .field("reconnect_backoff_max_ms", &self.reconnect_backoff_max_ms)
            .field("inference_endpoint", &self.inference_endpoint)
            .field("inference_model", &self.inference_model)
            .field("inference_timeout_ms", &self.inference_timeout_ms)
            .field(
                "inference_api_key",
                &self.inference_api_key.as_ref().map(|_| "<redacted>"),
            )
            .field("inference_headers", &self.inference_header_names())
            .field("slippage_bps", &self.slippage_bps)
            .field("slippage_vol_window_secs", &self.slippage_vol_window_secs)
            .field("slippage_vol_extra_cap_bps", &self.slippage_vol_extra_cap_bps)
            .field("strategy_mode", &self.strategy_mode)
            .field("inference_fallback", &self.inference_fallback)
            .field("inference_defer_max_n", &self.inference_defer_max_n)
            .field("force_exit_regime", &self.force_exit_regime)
            .field("runner_moonbag_percent", &self.runner_moonbag_percent)
            .field("priority_fee_micro_lamports", &self.priority_fee_micro_lamports)
            .field("compute_unit_limit", &self.compute_unit_limit)
            .field("tip_lamports", &self.tip_lamports)
            .field("jito_endpoint", &ids::redact_endpoint(&self.jito_endpoint))
            .field("discord_alerts", &self.discord_alerts)
            .field("discord_channel", &self.discord_channel)
            .field("sim_strict", &self.sim_strict)
            .field("alert_diverged_warn_n", &self.alert_diverged_warn_n)
            .field(
                "alert_risk_fallback_warn_pct",
                &self.alert_risk_fallback_warn_pct,
            )
            .field("alert_giveup_burst_n", &self.alert_giveup_burst_n)
            .field("alert_giveup_window_secs", &self.alert_giveup_window_secs)
            .field("parse_notes", &self.parse_notes)
            .finish()
    }
}

impl Settings {
    /// Read from the config file table. The file is the only source
    /// for tunables; `INFERENCE_API_KEY` is the single env exception
    /// (secret, never in the file). File typos surface as warnings,
    /// never silent defaults.
    pub fn from_file(file: Option<&toml::Table>) -> Self {
        let (mut merged, mut file_notes) = crate::config::overlay(file);
        if let Ok(key) = std::env::var("INFERENCE_API_KEY") {
            merged.insert("INFERENCE_API_KEY".to_string(), key);
        } else if let Some(key) = std::env::var("INFERENCE_API_KEY_FILE")
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            // Preferred: the bearer never enters the environment.
            merged.insert("INFERENCE_API_KEY".to_string(), key);
        }
        let mut settings = Self::from_map(&merged);
        // File notes first: they name the source of a value.
        file_notes.append(&mut settings.parse_notes);
        settings.parse_notes = file_notes;
        settings
    }

    /// Build from a map (tests avoid global env).
    pub fn from_map(vars: &HashMap<String, String>) -> Self {
        let mut notes = Vec::new();
        Self {
            ws_url: parse_wss_url(vars, &mut notes),
            http_url: parse_rpc_url(vars, &mut notes),
            fallback_rpc_urls: parse_fallback_rpc_urls(vars, &mut notes),
            yellowstone_endpoint: parse_yellowstone_url(vars, &mut notes),
            commitment: get_string(vars, "COMMITMENT", "confirmed"),
            start_balance: get_float(vars, "START_BALANCE", 100.0, &mut notes),
            fee_bps: get_float(vars, "FEE_BPS", 0.0, &mut notes),
            notional_usd: get_float(vars, "NOTIONAL_USD", 10.0, &mut notes),
            trail_percent: get_float(vars, "TRAIL_PERCENT", 5.0, &mut notes),
            runner_trail_percent: get_float(vars, "RUNNER_TRAIL_PERCENT", 20.0, &mut notes),
            liquidity_usd: get_float(vars, "LIQUIDITY_USD", 0.0, &mut notes),
            gate_window_secs: get_int(vars, "GATE_WINDOW_SECS", 10, &mut notes),
            gate_min_buyers: get_int(vars, "GATE_MIN_BUYERS", 5, &mut notes),
            gate_poll_ms: get_int(vars, "GATE_POLL_MS", 2000, &mut notes),
            max_open_positions: get_int(vars, "MAX_OPEN_POSITIONS", 3, &mut notes),
            max_daily_loss_usd: get_float(vars, "MAX_DAILY_LOSS_USD", 0.0, &mut notes),
            max_exposure_usd: get_float(vars, "MAX_EXPOSURE_USD", 0.0, &mut notes),
            max_expected_entry_gap_usd: get_float(
                vars,
                "MAX_EXPECTED_ENTRY_GAP_USD",
                0.0,
                &mut notes,
            ),
            max_top_holder_pct: get_float(vars, "MAX_TOP_HOLDER_PCT", 0.0, &mut notes),
            min_fill_rate_pct: get_float(vars, "MIN_FILL_RATE_PCT", 50.0, &mut notes),
            max_avg_exit_gap_usd: get_float(vars, "MAX_AVG_EXIT_GAP_USD", 0.0, &mut notes),
            max_avg_entry_gap_usd: get_float(vars, "MAX_AVG_ENTRY_GAP_USD", 0.0, &mut notes),
            hot_wallet_max_sol: get_float(vars, "HOT_WALLET_MAX_SOL", 0.0, &mut notes),
            mint_cooldown_secs: get_int(vars, "MINT_COOLDOWN_SECS", 3600, &mut notes),
            dev_min_age_secs: get_int(vars, "DEV_MIN_AGE_SECS", 3600, &mut notes),
            dev_max_prior_launches: get_int(vars, "DEV_MAX_PRIOR_LAUNCHES", 1, &mut notes),
            dev_screen_sample_txs: get_int(vars, "DEV_SCREEN_SAMPLE_TXS", 20, &mut notes),
            dev_sig_limit: get_int(vars, "DEV_SIG_LIMIT", 100, &mut notes),
            quote_usd: parse_quote_usd(vars, &mut notes),
            tick_interval_ms: get_int(vars, "TICK_INTERVAL_MS", 1000, &mut notes),
            vault_tick_interval_ms: get_int(vars, "VAULT_TICK_INTERVAL_MS", 5000, &mut notes),
            fetch_concurrency: get_int(vars, "FETCH_CONCURRENCY", 32, &mut notes),
            backfill_sig_limit: get_int(vars, "BACKFILL_SIG_LIMIT", 100, &mut notes),
            reconnect_backoff_max_ms: get_int(vars, "RECONNECT_BACKOFF_MAX_MS", 5000, &mut notes),
            inference_endpoint: get_string(vars, "INFERENCE_ENDPOINT", DEFAULT_INFERENCE_ENDPOINT),
            inference_model: get_string(vars, "INFERENCE_MODEL", "llama3.2:1b"),
            inference_timeout_ms: get_int(vars, "INFERENCE_TIMEOUT_MS", 300, &mut notes),
            inference_api_key: vars.get("INFERENCE_API_KEY").map(|s| Zeroizing::new(s.clone())),
            inference_headers: parse_inference_headers(vars, &mut notes),
            slippage_bps: get_int(vars, "SLIPPAGE_BPS", 200, &mut notes),
            slippage_vol_window_secs: get_int(vars, "SLIPPAGE_VOL_WINDOW_SECS", 0, &mut notes),
            slippage_vol_extra_cap_bps: get_int(vars, "SLIPPAGE_VOL_EXTRA_CAP_BPS", 0, &mut notes),
            strategy_mode: get_string(vars, "STRATEGY_MODE", "adaptive"),
            inference_fallback: get_string(vars, "INFERENCE_FALLBACK", "heuristic"),
            inference_defer_max_n: get_int(vars, "INFERENCE_DEFER_MAX_N", 1, &mut notes),
            force_exit_regime: get_string(vars, "FORCE_EXIT_REGIME", ""),
            runner_moonbag_percent: get_float(vars, "RUNNER_MOONBAG_PERCENT", 20.0, &mut notes),
            priority_fee_micro_lamports: get_int(vars, "PRIORITY_FEE_MICRO_LAMPORTS", 0, &mut notes),
            compute_unit_limit: get_int(vars, "COMPUTE_UNIT_LIMIT", 0, &mut notes),
            tip_lamports: get_int(vars, "JITO_TIP_LAMPORTS", 100_000, &mut notes),
            jito_endpoint: get_string(vars, "JITO_ENDPOINT", DEFAULT_JITO_ENDPOINT),
            discord_alerts: get_bool(vars, "DISCORD_ALERTS", true, &mut notes),
            discord_channel: vars
                .get("DISCORD_CHANNEL")
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            sim_strict: get_bool(vars, "SIM_STRICT", false, &mut notes),
            alert_diverged_warn_n: get_int(vars, "ALERT_DIVERGED_WARN_N", 3, &mut notes),
            alert_risk_fallback_warn_pct: get_int(vars, "ALERT_RISK_FALLBACK_WARN_PCT", 50, &mut notes),
            alert_giveup_burst_n: get_int(vars, "ALERT_GIVEUP_BURST_N", 3, &mut notes),
            alert_giveup_window_secs: get_int(vars, "ALERT_GIVEUP_WINDOW_SECS", 3600, &mut notes),
            parse_notes: notes,
        }
    }

    /// Warnings for bad values. Empty when sane.
    pub fn warnings(&self) -> Vec<String> {
        let mut out = self.parse_notes.clone();
        if self.slippage().is_none() {
            out.push(format!(
                "SLIPPAGE_BPS={} exceeds 100%; quotes fail closed (no guard can cover it — check the value, likely a typo)",
                self.slippage_bps
            ));
        }
        if self.slippage_vol_window_secs == 0 && self.slippage_vol_extra_cap_bps > 0 {
            out.push("SLIPPAGE_VOL_EXTRA_CAP_BPS>0 with SLIPPAGE_VOL_WINDOW_SECS=0: adaptive slippage is off (no window), the cap does nothing".to_string());
        }
        if self.slippage_vol_window_secs > 0 && self.slippage_vol_extra_cap_bps == 0 {
            out.push("SLIPPAGE_VOL_WINDOW_SECS>0 with SLIPPAGE_VOL_EXTRA_CAP_BPS=0: marks are tracked but the add-on is capped at 0 (static behavior)".to_string());
        }
        if self.strategy_mode.parse::<StrategyMode>().is_err() {
            out.push(format!(
                "STRATEGY_MODE='{}' unknown; falling back to adaptive",
                self.strategy_mode
            ));
        }
        if self.inference_fallback != "heuristic"
            && self.inference_fallback != "pass"
            && self.inference_fallback != "defer"
        {
            out.push(format!(
                "INFERENCE_FALLBACK='{}' unknown; staying heuristic (expected heuristic|pass|defer)",
                self.inference_fallback
            ));
        }
        if !self.force_exit_regime.is_empty()
            && self.force_exit_regime.parse::<ExitRegime>().is_err()
        {
            out.push(format!(
                "FORCE_EXIT_REGIME='{}' unknown; LLM selects (expected conservative|momentum|liquidity_sensitive, empty = LLM selects)",
                self.force_exit_regime
            ));
        }
        if self.inference_api_key.is_some() && is_plain_http_remote(&self.inference_endpoint) {
            out.push(
                "INFERENCE_API_KEY is set with a non-local plain-http INFERENCE_ENDPOINT: the bearer token travels in cleartext — use https or keep inference on localhost".to_string(),
            );
        }
        if self.runner_moonbag().is_none() {
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
        } else if self.fee_bps > 0.0 {
            out.push(format!(
                "FEE_BPS={} is commission-only on top of the 125bps venue fee already in FillModel fills (keep 0 unless charging extra)",
                self.fee_bps
            ));
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
        if self.max_exposure_usd < 0.0 || !self.max_exposure_usd.is_finite() {
            out.push(format!(
                "MAX_EXPOSURE_USD={} is invalid; exposure cap disabled",
                self.max_exposure_usd
            ));
        } else if self.max_exposure_usd > 0.0 && self.max_exposure_usd < self.notional_usd {
            out.push(format!(
                "MAX_EXPOSURE_USD={} is below NOTIONAL_USD={}: one ticket never fits, the cap blocks every open",
                self.max_exposure_usd, self.notional_usd
            ));
        }
        if self.max_expected_entry_gap_usd < 0.0 || !self.max_expected_entry_gap_usd.is_finite() {
            out.push(format!(
                "MAX_EXPECTED_ENTRY_GAP_USD={} is invalid; entry bleed cap disabled",
                self.max_expected_entry_gap_usd
            ));
        }
        if self.max_top_holder_pct < 0.0 || !self.max_top_holder_pct.is_finite() {
            out.push(format!(
                "MAX_TOP_HOLDER_PCT={} is invalid; holder concentration cap disabled",
                self.max_top_holder_pct
            ));
        } else if self.max_top_holder_pct > 100.0 {
            out.push(format!(
                "MAX_TOP_HOLDER_PCT={} exceeds 100%: no holding can refuse, the cap never fires",
                self.max_top_holder_pct
            ));
        }
        if self.min_fill_rate_pct < 0.0
            || self.min_fill_rate_pct > 100.0
            || !self.min_fill_rate_pct.is_finite()
        {
            out.push(format!(
                "MIN_FILL_RATE_PCT={} is invalid (want 0-100); fill-rate trip disabled",
                self.min_fill_rate_pct
            ));
        }
        if self.max_avg_exit_gap_usd < 0.0 || !self.max_avg_exit_gap_usd.is_finite() {
            out.push(format!(
                "MAX_AVG_EXIT_GAP_USD={} is invalid; exit-gap trip disabled",
                self.max_avg_exit_gap_usd
            ));
        }
        if self.max_avg_entry_gap_usd < 0.0 || !self.max_avg_entry_gap_usd.is_finite() {
            out.push(format!(
                "MAX_AVG_ENTRY_GAP_USD={} is invalid; entry-gap trip disabled",
                self.max_avg_entry_gap_usd
            ));
        }
        if self.hot_wallet_max_sol < 0.0 || !self.hot_wallet_max_sol.is_finite() {
            out.push(format!(
                "HOT_WALLET_MAX_SOL={} is invalid; hot-wallet cap disabled",
                self.hot_wallet_max_sol
            ));
        }
        if self.gate_min_buyers == 0 {
            out.push("GATE_MIN_BUYERS=0 behaves as 1 (quorum floor)".to_string());
        }
        if self.commitment == "processed" {
            out.push(
                "COMMITMENT=processed is optimistic (reorgable prints): prefer confirmed for exits"
                    .to_string(),
            );
        } else if !matches!(self.commitment.as_str(), "confirmed" | "finalized") {
            out.push(format!(
                "COMMITMENT='{}' unknown; startup refuses it (expected processed|confirmed|finalized)",
                self.commitment
            ));
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
                "INFERENCE_TIMEOUT_MS=0 is clamped to the 50ms floor at build; expect near-total fallback (set 300 for the default budget)".to_string(),
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

    /// Typed slippage guard (`None` when >=100% or unrepresentable).
    /// Prefer this over raw `slippage_bps` in new code.
    pub fn slippage(&self) -> Option<crate::money::Bps> {
        u32::try_from(self.slippage_bps)
            .ok()
            .and_then(crate::money::Bps::new)
    }

    /// Typed runner moonbag share (`None` when outside 0–100).
    pub fn runner_moonbag(&self) -> Option<crate::money::Percent> {
        crate::money::Percent::new(self.runner_moonbag_percent)
    }

    /// Hot-wallet ceiling in lamports (`0` = off). Negative, NaN, and
    /// infinite values read as off (warned in `warnings`).
    pub fn hot_wallet_cap_lamports(&self) -> u64 {
        if !self.hot_wallet_max_sol.is_finite() || self.hot_wallet_max_sol <= 0.0 {
            return 0;
        }
        (self.hot_wallet_max_sol * ids::LAMPORTS_PER_SOL as f64).min(u64::MAX as f64) as u64
    }

    /// Extra header names only (values may carry routing secrets and
    /// must never reach logs; Debug prints this, not the pairs).
    pub fn inference_header_names(&self) -> Vec<&str> {
        self.inference_headers
            .iter()
            .map(|(n, _)| n.as_str())
            .collect()
    }

    /// Fatal misconfig: values that guarantee no-opens or unguarded
    /// spend. Called at boot in every mode (paper must not run blind
    /// either); `warnings` keeps the soft advisories.
    pub fn validate(&self) -> Result<(), String> {
        if !self.notional_usd.is_finite() || self.notional_usd <= 0.0 {
            return Err(format!(
                "NOTIONAL_USD={} is not positive; no position can open",
                self.notional_usd
            ));
        }
        // Refuse at or above 100%: `Bps` admits `0..=10_000` and
        // `is_full` marks the 100% edge, so anything else (over the
        // edge or unrepresentable as u32) refuses — exactly the old
        // `>= BPS_DENOMINATOR` gate, message unchanged.
        let slippage_ok = self.slippage().is_some_and(|b| !b.is_full());
        if !slippage_ok {
            return Err(format!(
                "SLIPPAGE_BPS={} (>=100%): buys would authorize 2x spend while every sell fails closed",
                self.slippage_bps
            ));
        }
        if self.fetch_concurrency == 0 {
            return Err(
                "FETCH_CONCURRENCY=0 blinds ingestion (zero fetch permits drop every candidate)".to_string(),
            );
        }
        Ok(())
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
        Some(v) => match v.parse::<f64>() {
            // `nan`/`inf` parse Ok — never boot them.
            Ok(x) if x.is_finite() => x,
            _ => {
                notes.push(format!(
                    "{key}='{v}' is not a finite number; using default {default}"
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

/// Canonical WebSocket URL (`MAINNET_WSS_RPC_URL`; empty = public default).
fn parse_wss_url(vars: &HashMap<String, String>, notes: &mut Vec<String>) -> String {
    let clean = |v: &String| v.replace('\\', "").trim().to_string();
    if vars.contains_key("MAINNET_WSS_RPC_URL")
        && vars.get("MAINNET_WSS_RPC_URL").is_some_and(|v| clean(v).is_empty())
    {
        notes.push(
            "MAINNET_WSS_RPC_URL is blank; using the public endpoint (check for typos)".to_string(),
        );
    }
    match vars.get("MAINNET_WSS_RPC_URL").map(clean).filter(|v| !v.is_empty()) {
        Some(url) => url,
        None => listener::types::DEFAULT_WS_RPC_URL.to_string(),
    }
}

/// Canonical HTTP RPC URL (`MAINNET_RPC_URL`; empty = public default).
fn parse_rpc_url(vars: &HashMap<String, String>, notes: &mut Vec<String>) -> String {
    let clean = |v: &String| v.replace('\\', "").trim().to_string();
    // Blank set var is a typo, not unset.
    if vars.contains_key("MAINNET_RPC_URL")
        && vars.get("MAINNET_RPC_URL").is_some_and(|v| clean(v).is_empty())
    {
        notes.push(
            "MAINNET_RPC_URL is blank; using the public endpoint (check for typos)".to_string(),
        );
    }
    match vars.get("MAINNET_RPC_URL").map(clean).filter(|v| !v.is_empty()) {
        Some(url) => url,
        None => listener::types::DEFAULT_HTTP_RPC_URL.to_string(),
    }
}

/// Comma-separated read-failover endpoints (`MAINNET_RPC_URLS_FALLBACK`
/// or TOML `fallback_rpc_urls`); empty = primary only (today's shape).
fn parse_fallback_rpc_urls(vars: &HashMap<String, String>, notes: &mut Vec<String>) -> Vec<String> {
    let clean = |v: &String| v.replace('\\', "").trim().to_string();
    let Some(raw) = vars
        .get("MAINNET_RPC_URLS_FALLBACK")
        .map(clean)
        .filter(|v| !v.is_empty())
    else {
        return Vec::new();
    };
    let urls: Vec<String> = raw
        .split(',')
        .map(|s| clean(&s.to_string()))
        .filter(|u| !u.is_empty())
        .collect();
    if urls.is_empty() {
        notes
            .push("MAINNET_RPC_URLS_FALLBACK is set but empty; reads run primary-only".to_string());
    }
    urls
}

/// Yellowstone gRPC endpoint. Empty (unset) selects WS ingestion;
/// non-empty must look like an `http(s)://host` URL (gRPC dials off
/// the scheme; full validation happens at connect). No legacy alias,
/// no default: unset is a valid mode.
fn parse_yellowstone_url(vars: &HashMap<String, String>, notes: &mut Vec<String>) -> String {
    let clean = |v: &String| v.replace('\\', "").trim().to_string();
    match vars
        .get("YELLOWSTONE_ENDPOINT")
        .map(clean)
        .filter(|v| !v.is_empty())
    {
        Some(url) => {
            let rest = url
                .strip_prefix("https://")
                .or_else(|| url.strip_prefix("http://"));
            match rest {
                Some(host) if !host.split(['/', '?', '#']).next().unwrap_or("").is_empty() => url,
                _ => {
                    notes.push(format!(
                        "YELLOWSTONE_ENDPOINT='{url}' is not an http/https URL; ignoring (WS ingestion)"
                    ));
                    String::new()
                }
            }
        }
        None => String::new(),
    }
}

/// RFC 7230 token: the name side of a header.
fn is_header_name(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| matches!(
            b,
            b'0'..=b'9'
                | b'A'..=b'Z'
                | b'a'..=b'z'
                | b'!'
                | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        ))
}

/// Header value: non-empty, no controls or DEL (obs-text allowed).
/// The engine re-validates strictly at build; this stays permissive
/// so parse never rejects what the transport would accept.
fn is_header_value(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b >= 32 && b != 127)
}

/// Optional extra inference headers (`INFERENCE_HEADERS`) as
/// `"Name: value, ..."` (first-colon split, so values may contain
/// `:`; values must not contain `,`). Empty/unset sends none —
/// providers that need no headers are unaffected. Unparseable and
/// transport-owned entries are skipped with a note naming the header
/// only (never the value). Duplicates resolve last-wins.
fn parse_inference_headers(
    vars: &HashMap<String, String>,
    notes: &mut Vec<String>,
) -> Vec<(String, String)> {
    let raw = match vars.get("INFERENCE_HEADERS") {
        None => return Vec::new(),
        Some(raw) if raw.trim().is_empty() => return Vec::new(),
        Some(raw) => raw.clone(),
    };
    let mut out: Vec<(String, String)> = Vec::new();
    for (idx, item) in raw.split(',').enumerate() {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (name, value) = match item.split_once(':') {
            Some((n, v)) => (n.trim().to_string(), v.trim().to_string()),
            None => {
                notes.push(format!(
                    "INFERENCE_HEADERS entry #{} ignored (want 'Name: value')",
                    idx + 1
                ));
                continue;
            }
        };
        if name.is_empty() || !is_header_name(&name) {
            notes.push(format!(
                "INFERENCE_HEADERS header '{name}' has an invalid name; ignoring"
            ));
            continue;
        }
        // Single source lives in `risk_engine` (same list the
        // builder enforces); auth stays in `INFERENCE_API_KEY` env.
        if risk_engine::is_owned_inference_header(&name) {
            notes.push(format!(
                "INFERENCE_HEADERS header '{name}' refused (transport-owned; auth stays in INFERENCE_API_KEY env, the user agent is owned)"
            ));
            continue;
        }
        if !is_header_value(&value) {
            notes.push(format!(
                "INFERENCE_HEADERS header '{name}' has an invalid value; ignoring"
            ));
            continue;
        }
        if let Some(pos) = out.iter().position(|(n, _)| n.eq_ignore_ascii_case(&name)) {
            out.remove(pos);
        }
        out.push((name, value));
    }
    out
}

/// Manual entry quote: only positive finite is honored.
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

/// True for plain-http endpoints outside localhost: bearer tokens
/// sent there travel in cleartext. Local servers are exempt.
/// Shared with the live boot gate (refuses there, warns elsewhere).
pub(crate) fn is_plain_http_remote(endpoint: &str) -> bool {
    let rest = match endpoint.trim().strip_prefix("http://") {
        Some(r) => r,
        None => return false,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Strip a port (`127.0.0.1:11434`); bracketed IPv6 keeps its
    // colons (`[::1]:11434`).
    let host = match authority.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    };
    let host = host.strip_suffix('.').unwrap_or(host);
    !(host.is_empty()
        || host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
        || host == "::1")
}

/// Unix seconds; `0` when the clock fails (never panics).
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
    use std::sync::{Mutex, OnceLock};

    /// Serializes env-mutating tests (process-global env shared by
    /// parallel test threads).
    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn fallback_rpc_urls_parse_comma_list() {
        let mut vars = HashMap::new();
        vars.insert(
            "MAINNET_RPC_URLS_FALLBACK".to_string(),
            " https://a.example , ,https://b.example ".to_string(),
        );
        let s = Settings::from_map(&vars);
        assert_eq!(
            s.fallback_rpc_urls,
            vec![
                "https://a.example".to_string(),
                "https://b.example".to_string()
            ]
        );
        // Blank value = unset, not one empty endpoint.
        let mut blank = HashMap::new();
        blank.insert("MAINNET_RPC_URLS_FALLBACK".to_string(), "  ".to_string());
        let s = Settings::from_map(&blank);
        assert!(s.fallback_rpc_urls.is_empty());
    }

    #[test]
    fn defaults_are_sane() {
        let s = Settings::from_map(&HashMap::new());
        assert_eq!(s.commitment, "confirmed");
        assert!(s.fallback_rpc_urls.is_empty());
        assert_eq!(s.start_balance, 100.0);
        assert_eq!(s.fee_bps, 0.0);
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
        assert_eq!(s.fetch_concurrency, 32);
        assert_eq!(s.backfill_sig_limit, 100);
        assert_eq!(s.reconnect_backoff_max_ms, 5000);
        assert!(s.inference_headers.is_empty());
        assert!(!s.sim_strict);
        assert!(s.parse_notes.is_empty());
    }

    #[test]
    fn rpc_urls_prefer_mainnet_vars_and_strip_escapes() {
        // Canonical var wins; retired aliases are ignored entirely.
        let s = Settings::from_map(&vars(&[("MAINNET_RPC_URL", "https://helius/abc")]));
        assert_eq!(s.http_url, "https://helius/abc");
        assert!(s.parse_notes.is_empty());
        // Backslash escapes stripped.
        let s = Settings::from_map(&vars(&[("MAINNET_RPC_URL", "https:\\\\/\\\\/helius")]));
        assert_eq!(s.http_url, "https://helius");
        // Retired aliases no longer honored: unset = public default.
        let s = Settings::from_map(&vars(&[("HTTP_URL", "https://legacy/xyz")]));
        assert_eq!(s.http_url, listener::types::DEFAULT_HTTP_RPC_URL);
        let s = Settings::from_map(&vars(&[("WS_URL", "wss://legacy/xyz")]));
        assert_eq!(s.ws_url, listener::types::DEFAULT_WS_RPC_URL);
        // Unset = public default.
        let s = Settings::from_map(&HashMap::new());
        assert_eq!(s.http_url, listener::types::DEFAULT_HTTP_RPC_URL);
        assert_eq!(s.ws_url, listener::types::DEFAULT_WS_RPC_URL);
        // Canonical WSS wins.
        let s = Settings::from_map(&vars(&[("MAINNET_WSS_RPC_URL", "wss://helius/abc")]));
        assert_eq!(s.ws_url, "wss://helius/abc");
    }

    #[test]
    fn exposure_and_hot_wallet_caps_parse_and_warn() {
        let s = Settings::from_map(&HashMap::new());
        assert_eq!(s.max_exposure_usd, 0.0, "off by default");
        assert_eq!(s.hot_wallet_max_sol, 0.0, "off by default");
        assert_eq!(s.hot_wallet_cap_lamports(), 0);

        let s = Settings::from_map(&vars(&[
            ("MAX_EXPOSURE_USD", "150.0"),
            ("HOT_WALLET_MAX_SOL", "1.5"),
            ("NOTIONAL_USD", "20.0"),
        ]));
        assert_eq!(s.max_exposure_usd, 150.0);
        assert_eq!(s.hot_wallet_cap_lamports(), 1_500_000_000);
        assert!(
            !s.warnings()
                .iter()
                .any(|w| w.contains("MAX_EXPOSURE_USD=") || w.contains("HOT_WALLET_MAX_SOL=")),
            "{:?}",
            s.warnings()
        );

        // A cap below one ticket never opens: say so.
        let w = Settings::from_map(&vars(&[("MAX_EXPOSURE_USD", "5.0")]))
            .warnings()
            .join("\n");
        assert!(w.contains("below NOTIONAL_USD"), "{w}");
        // Garbage reads as off, loudly — never silently armed.
        let s = Settings::from_map(&vars(&[("HOT_WALLET_MAX_SOL", "-3")]));
        assert_eq!(s.hot_wallet_cap_lamports(), 0);
        assert!(
            s.warnings()
                .iter()
                .any(|w| w.contains("HOT_WALLET_MAX_SOL=-3")),
            "{:?}",
            s.warnings()
        );
        let s = Settings::from_map(&vars(&[("MAX_EXPOSURE_USD", "-1")]));
        assert!(
            s.warnings()
                .iter()
                .any(|w| w.contains("MAX_EXPOSURE_USD=-1")),
            "{:?}",
            s.warnings()
        );
    }

    #[test]
    fn entry_bleed_cap_parses_and_warns() {
        let s = Settings::from_map(&HashMap::new());
        assert_eq!(s.max_expected_entry_gap_usd, 0.0, "off by default");
        let s = Settings::from_map(&vars(&[("MAX_EXPECTED_ENTRY_GAP_USD", "0.75")]));
        assert_eq!(s.max_expected_entry_gap_usd, 0.75);
        assert!(
            !s.warnings()
                .iter()
                .any(|w| w.contains("MAX_EXPECTED_ENTRY_GAP_USD")),
            "{:?}",
            s.warnings()
        );
        // Garbage reads as disabled, loudly — never silently armed.
        let s = Settings::from_map(&vars(&[("MAX_EXPECTED_ENTRY_GAP_USD", "-1")]));
        assert!(
            s.warnings()
                .iter()
                .any(|w| w.contains("MAX_EXPECTED_ENTRY_GAP_USD=-1")),
            "{:?}",
            s.warnings()
        );
    }

    #[test]
    fn holder_concentration_cap_parses_and_warns() {
        let s = Settings::from_map(&HashMap::new());
        assert_eq!(s.max_top_holder_pct, 0.0, "off by default");
        let s = Settings::from_map(&vars(&[("MAX_TOP_HOLDER_PCT", "40")]));
        assert_eq!(s.max_top_holder_pct, 40.0);
        assert!(
            !s.warnings()
                .iter()
                .any(|w| w.contains("MAX_TOP_HOLDER_PCT")),
            "{:?}",
            s.warnings()
        );
        // Garbage reads as disabled, loudly — never silently armed.
        let s = Settings::from_map(&vars(&[("MAX_TOP_HOLDER_PCT", "-1")]));
        assert!(
            s.warnings()
                .iter()
                .any(|w| w.contains("MAX_TOP_HOLDER_PCT=-1")),
            "{:?}",
            s.warnings()
        );
        // A cap above 100% never fires: say so.
        let w = Settings::from_map(&vars(&[("MAX_TOP_HOLDER_PCT", "120")]))
            .warnings()
            .join("\n");
        assert!(w.contains("never fires"), "{w}");
    }

    #[test]
    fn execution_trip_knobs_parse_and_warn() {
        let s = Settings::from_map(&HashMap::new());
        assert_eq!(s.min_fill_rate_pct, 50.0, "armed by default");
        assert_eq!(s.max_avg_exit_gap_usd, 0.0, "off by default");
        assert_eq!(s.max_avg_entry_gap_usd, 0.0, "off by default");
        let s = Settings::from_map(&vars(&[
            ("MIN_FILL_RATE_PCT", "75"),
            ("MAX_AVG_EXIT_GAP_USD", "0.5"),
            ("MAX_AVG_ENTRY_GAP_USD", "0.25"),
        ]));
        assert_eq!(s.min_fill_rate_pct, 75.0);
        assert_eq!(s.max_avg_exit_gap_usd, 0.5);
        assert_eq!(s.max_avg_entry_gap_usd, 0.25);
        assert!(
            !s.warnings().iter().any(|w| w.contains("MIN_FILL_RATE_PCT")
                || w.contains("MAX_AVG_EXIT_GAP_USD")
                || w.contains("MAX_AVG_ENTRY_GAP_USD")),
            "{:?}",
            s.warnings()
        );
        // Out-of-range reads as disabled, loudly.
        for (key, val) in [
            ("MIN_FILL_RATE_PCT", "-1"),
            ("MIN_FILL_RATE_PCT", "101"),
            ("MAX_AVG_EXIT_GAP_USD", "-0.5"),
            ("MAX_AVG_ENTRY_GAP_USD", "-0.5"),
        ] {
            let w = Settings::from_map(&vars(&[(key, val)]))
                .warnings()
                .join("\n");
            assert!(w.contains(key), "{key}={val}: {w}");
        }
    }

    #[test]
    fn inference_fallback_parses_and_warns() {
        let s = Settings::from_map(&HashMap::new());
        assert_eq!(
            s.inference_fallback, "heuristic",
            "validated bands by default"
        );
        let s = Settings::from_map(&vars(&[("INFERENCE_FALLBACK", "pass")]));
        assert_eq!(s.inference_fallback, "pass");
        assert!(
            !s.warnings().iter().any(|w| w.contains("INFERENCE_FALLBACK")),
            "{:?}",
            s.warnings()
        );
        let s = Settings::from_map(&vars(&[("INFERENCE_FALLBACK", "defer")]));
        assert_eq!(s.inference_fallback, "defer");
        assert!(s.inference_defer_max_n > 0, "bare defer defaults to one retry");
        assert!(
            !s.warnings().iter().any(|w| w.contains("INFERENCE_FALLBACK")),
            "{:?}",
            s.warnings()
        );
        // Unknown stays heuristic, loudly — never a hotter fallback.
        let s = Settings::from_map(&vars(&[("INFERENCE_FALLBACK", "yolo")]));
        assert_eq!(s.inference_fallback, "yolo");
        assert!(
            s.warnings()
                .iter()
                .any(|w| w.contains("INFERENCE_FALLBACK='yolo'")),
            "{:?}",
            s.warnings()
        );
    }

    #[test]
    fn inference_defer_max_n_parses() {
        let s = Settings::from_map(&HashMap::new());
        assert_eq!(s.inference_defer_max_n, 1, "one defer by default");
        let s = Settings::from_map(&vars(&[("INFERENCE_DEFER_MAX_N", "3")]));
        assert_eq!(s.inference_defer_max_n, 3);
        // 0 = off: the defer policy behaves as heuristic at once.
        let s = Settings::from_map(&vars(&[("INFERENCE_DEFER_MAX_N", "0")]));
        assert_eq!(s.inference_defer_max_n, 0);
    }

    #[test]
    fn force_exit_regime_parses_and_warns() {
        let s = Settings::from_map(&HashMap::new());
        assert_eq!(s.force_exit_regime, "", "empty = LLM selects");
        let s = Settings::from_map(&vars(&[("FORCE_EXIT_REGIME", "momentum")]));
        assert_eq!(s.force_exit_regime, "momentum");
        assert!(
            !s.warnings().iter().any(|w| w.contains("FORCE_EXIT_REGIME")),
            "{:?}",
            s.warnings()
        );
        // Unknown non-empty warns loudly and selects nothing (LLM decides).
        let w = Settings::from_map(&vars(&[("FORCE_EXIT_REGIME", "turbo")]))
            .warnings()
            .join("\n");
        assert!(w.contains("FORCE_EXIT_REGIME='turbo'"), "{w}");
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
            ("FETCH_CONCURRENCY", "8"),
            ("INFERENCE_API_KEY", "secret"),
            ("SIM_STRICT", "true"),
        ]));
        assert_eq!(s.start_balance, 250.5);
        assert_eq!(s.gate_min_buyers, 5);
        assert_eq!(s.quote_usd, Some(0.000001));
        assert_eq!(s.commitment, "finalized");
        assert_eq!(s.slippage_bps, 500);
        assert_eq!(s.tip_lamports, 200_000);
        assert_eq!(s.fetch_concurrency, 8);
        assert_eq!(s.backfill_sig_limit, 100);
        assert_eq!(s.reconnect_backoff_max_ms, 5000);
        assert_eq!(
            s.inference_api_key.as_ref().map(|k| k.as_str()),
            Some("secret")
        );
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
    fn degenerate_values_warn() {
        let s = Settings::from_map(&vars(&[
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
    fn half_armed_adaptive_slippage_warns() {
        // Defaults: off, silent.
        let s = Settings::from_map(&HashMap::new());
        assert_eq!(s.slippage_vol_window_secs, 0);
        assert_eq!(s.slippage_vol_extra_cap_bps, 0);
        assert!(!s.warnings().iter().any(|w| w.contains("SLIPPAGE_VOL")));
        // Cap without a window (and vice versa) warns loudly.
        let s = Settings::from_map(&vars(&[("SLIPPAGE_VOL_EXTRA_CAP_BPS", "150")]));
        assert!(s.warnings().iter().any(|w| w.contains("SLIPPAGE_VOL_EXTRA_CAP_BPS>0")), "{:?}", s.warnings());
        let s = Settings::from_map(&vars(&[("SLIPPAGE_VOL_WINDOW_SECS", "60")]));
        assert!(s.warnings().iter().any(|w| w.contains("SLIPPAGE_VOL_WINDOW_SECS>0")), "{:?}", s.warnings());
        // Fully armed: parses, no warning.
        let s = Settings::from_map(&vars(&[
            ("SLIPPAGE_VOL_WINDOW_SECS", "60"),
            ("SLIPPAGE_VOL_EXTRA_CAP_BPS", "150"),
        ]));
        assert_eq!(s.slippage_vol_window_secs, 60);
        assert_eq!(s.slippage_vol_extra_cap_bps, 150);
        assert!(!s.warnings().iter().any(|w| w.contains("SLIPPAGE_VOL")));
    }

    #[test]
    fn bearer_over_plain_http_warns() {
        // No key set: nothing to leak, no warning.
        let s = Settings::from_map(&vars(&[("INFERENCE_ENDPOINT", "http://llm.example/v1")]));
        assert!(!s.warnings().iter().any(|w| w.contains("cleartext")));
        // Key + remote plain http: warn.
        let s = Settings::from_map(&vars(&[
            ("INFERENCE_ENDPOINT", "http://llm.example/v1"),
            ("INFERENCE_API_KEY", "k"),
        ]));
        assert!(s.warnings().iter().any(|w| w.contains("cleartext")));
        // Key + https or localhost: silent.
        let mut https = s.clone();
        https.inference_endpoint = "https://llm.example/v1".to_string();
        assert!(!https.warnings().iter().any(|w| w.contains("cleartext")));
        for local in [
            "http://127.0.0.1:11434/v1",
            "http://localhost:11434/v1",
            "http://[::1]:11434/v1",
        ] {
            let mut ep = s.clone();
            ep.inference_endpoint = local.to_string();
            assert!(!ep.warnings().iter().any(|w| w.contains("cleartext")), "{local}");
        }
    }

    #[test]
    fn file_is_the_only_source_for_tunables() {
        let file: toml::Table = "[trading]\nnotional_usd = 42.0\n[gate]\ngate_window_sec = 5\n"
            .parse()
            .expect("test toml");
        let s = Settings::from_file(Some(&file));
        assert_eq!(s.notional_usd, 42.0);
        assert!(s.warnings().iter().any(|w| w.contains("[gate].gate_window_sec unknown")), "{:?}", s.warnings());
        // No file: pure defaults.
        let s = Settings::from_file(None);
        assert_eq!(s.notional_usd, 10.0);
    }

    #[test]
    fn inference_key_stays_env_only() {
        // The single sanctioned env exception: a bearer token must
        // never sit in the config file.
        let _lock = env_lock().lock().expect("lock");
        let file: toml::Table = "[risk]\n".parse().expect("test toml");
        let saved = std::env::var("INFERENCE_API_KEY").ok();
        unsafe { std::env::set_var("INFERENCE_API_KEY", "test-secret") };
        let s = Settings::from_file(Some(&file));
        assert_eq!(
            s.inference_api_key.as_ref().map(|k| k.as_str()),
            Some("test-secret")
        );
        match saved {
            Some(v) => unsafe { std::env::set_var("INFERENCE_API_KEY", v) },
            None => unsafe { std::env::remove_var("INFERENCE_API_KEY") },
        }
        // ... and the file cannot provide it (no such key exists).
        assert!(
            !crate::config::KEYMAP.iter().any(|(_, _, flat)| *flat == "INFERENCE_API_KEY"),
            "inference key must never gain a file mapping"
        );
    }

    #[test]
    fn inference_key_file_fallback_avoids_export() {
        // Preferred path: bearer from a file so it never enters the
        // environment (`ps e`, shell history stay clean).
        let _lock = env_lock().lock().expect("lock");
        let file: toml::Table = "[risk]\n".parse().expect("test toml");
        let saved = std::env::var("INFERENCE_API_KEY").ok();
        let saved_file = std::env::var("INFERENCE_API_KEY_FILE").ok();
        unsafe { std::env::remove_var("INFERENCE_API_KEY") };
        let dir = std::env::temp_dir().join(format!(
            "inference-key-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("key.txt");
        std::fs::write(&path, "  file-secret\n").expect("write");
        unsafe { std::env::set_var("INFERENCE_API_KEY_FILE", &path) };
        let s = Settings::from_file(Some(&file));
        assert_eq!(
            s.inference_api_key.as_ref().map(|k| k.as_str()),
            Some("file-secret")
        );
        unsafe { std::env::remove_var("INFERENCE_API_KEY_FILE") };
        match saved {
            Some(v) => unsafe { std::env::set_var("INFERENCE_API_KEY", v) },
            None => unsafe { std::env::remove_var("INFERENCE_API_KEY") },
        }
        if let Some(v) = saved_file {
            unsafe { std::env::set_var("INFERENCE_API_KEY_FILE", v) };
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn yellowstone_token_stays_env_only() {
        // Same rule as the inference key: the geyser x-token must never
        // sit in the config file (it authenticates and bills).
        assert!(
            !crate::config::KEYMAP
                .iter()
                .any(|(_, _, flat)| *flat == "YELLOWSTONE_TOKEN"),
            "geyser token must never gain a file mapping"
        );
    }

    #[test]
    fn yellowstone_endpoint_parses() {
        // Unset selects WS; garbage fails closed to WS with a warning.
        let s = Settings::from_file(None);
        assert_eq!(s.yellowstone_endpoint, "");
        let file: toml::Table = "[network]\nyellowstone_endpoint = \"https://grr.example:443\"\n"
            .parse()
            .expect("test toml");
        let s = Settings::from_file(Some(&file));
        assert_eq!(s.yellowstone_endpoint, "https://grr.example:443");
        assert!(s.warnings().iter().all(|w| !w.contains("YELLOWSTONE")));
        let bad: toml::Table = "[network]\nyellowstone_endpoint = \"notaurl\"\n"
            .parse()
            .expect("test toml");
        let s = Settings::from_file(Some(&bad));
        assert_eq!(s.yellowstone_endpoint, "");
        assert!(
            s.warnings()
                .iter()
                .any(|w| w.contains("YELLOWSTONE_ENDPOINT")),
            "{:?}",
            s.warnings()
        );
    }

    #[test]
    fn fatal_misconfig_refuses_at_validate() {
        let ok = Settings::from_map(&HashMap::new());
        assert!(ok.validate().is_ok());
        for (k, v) in [("NOTIONAL_USD", "0"), ("NOTIONAL_USD", "-5")] {
            let s = Settings::from_map(&vars(&[(k, v)]));
            assert!(s.validate().is_err(), "{k}={v}");
        }
        let s = Settings::from_map(&vars(&[("SLIPPAGE_BPS", "10000")]));
        assert!(s.validate().is_err());
        // Zero fetch permits drop every candidate: refuse, don't run blind.
        let s = Settings::from_map(&vars(&[("FETCH_CONCURRENCY", "0")]));
        assert!(s.validate().is_err());
        // Warnings still fire alongside (soft copy for logs).
        let s = Settings::from_map(&vars(&[("SLIPPAGE_BPS", "20000")]));
        assert!(s.validate().is_err());
        assert!(s.warnings().iter().any(|w| w.contains("SLIPPAGE_BPS=20000")));
    }

    #[test]
    fn bogus_commitment_warns_early() {
        // `main` refuses unknown commitments at boot; the warning must
        // already fire here so paper runs notice it in `warnings()`.
        let s = Settings::from_map(&vars(&[("COMMITMENT", "bogus")]));
        let w = s.warnings().join("\n");
        assert!(w.contains("COMMITMENT='bogus' unknown"), "{w}");
        let s = Settings::from_map(&HashMap::new());
        assert!(!s.warnings().iter().any(|x| x.contains("unknown; startup refuses")));
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
    fn debug_redacts_url_queries() {
        // Helius-style URLs carry ?api-key=: Debug must keep the host
        // (debuggable) and drop the query (secret).
        let s = Settings::from_map(&vars(&[
            ("MAINNET_RPC_URL", "https://mainnet.helius-rpc.com/?api-key=K1"),
            ("MAINNET_WSS_RPC_URL", "wss://mainnet.helius-rpc.com/?api-key=K2"),
            ("JITO_ENDPOINT", "https://jito.example/?key=K3"),
        ]));
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("K1"), "{dbg}");
        assert!(!dbg.contains("K2"), "{dbg}");
        assert!(!dbg.contains("K3"), "{dbg}");
        assert!(!dbg.contains("api-key"), "{dbg}");
        assert!(dbg.contains("mainnet.helius-rpc.com"), "{dbg}");
        // Transport still uses the raw values.
        assert_eq!(s.http_url, "https://mainnet.helius-rpc.com/?api-key=K1");
        assert_eq!(s.ws_url, "wss://mainnet.helius-rpc.com/?api-key=K2");
        assert_eq!(s.jito_endpoint, "https://jito.example/?key=K3");
    }

    #[test]
    fn inference_headers_parse_routing_pair() {
        // OpenCode Go shape: session routing header rides verbatim.
        let s = Settings::from_map(&vars(&[(
            "INFERENCE_HEADERS",
            "x-opencode-session: sess-123, X-Custom-Flag: fast",
        )]));
        assert_eq!(
            s.inference_headers,
            vec![
                ("x-opencode-session".to_string(), "sess-123".to_string()),
                ("X-Custom-Flag".to_string(), "fast".to_string()),
            ]
        );
        assert!(s.parse_notes.is_empty());
        // Values may contain colons (first-colon split).
        let s = Settings::from_map(&vars(&[("INFERENCE_HEADERS", "x-k: a:b")]));
        assert_eq!(
            s.inference_headers,
            vec![("x-k".to_string(), "a:b".to_string())]
        );
    }

    #[test]
    fn inference_headers_refuse_auth_transport_and_malformed() {
        let s = Settings::from_map(&vars(&[(
            "INFERENCE_HEADERS",
            "authorization: s3cr3t, User-Agent: spoof, content-type: x, no-colon-here, : novalue, bad name!: v, x-bad-value: a\nb, x-ok: good",
        )]));
        assert_eq!(
            s.inference_headers,
            vec![("x-ok".to_string(), "good".to_string())]
        );
        let w = s.warnings().join("\n");
        assert!(w.contains("authorization"), "{w}");
        assert!(w.contains("User-Agent"), "{w}");
        // Entry numbers surface instead of echoing unparseable text.
        assert!(w.contains("entry #4"), "{w}");
        assert!(!w.contains("no-colon-here"), "{w}");
        assert!(w.contains("x-bad-value"), "{w}");
        // Values never reach logs, even in refusal notes.
        assert!(!w.contains("s3cr3t"), "{w}");
        assert!(!w.contains("good"), "{w}");
    }

    #[test]
    fn inference_headers_dedupe_last_wins() {
        let s = Settings::from_map(&vars(&[("INFERENCE_HEADERS", "x-sess: first, X-Sess: second")]));
        assert_eq!(s.inference_headers.len(), 1);
        assert_eq!(s.inference_headers[0].1, "second");
    }

    #[test]
    fn inference_headers_from_file_table() {
        // End-to-end: KEYMAP carries the key from [risk] into the map.
        let file: toml::Table = "[risk]\ninference_headers = \"x-opencode-session: sess-9\"\n"
            .parse()
            .expect("test toml");
        let s = Settings::from_file(Some(&file));
        assert_eq!(
            s.inference_headers,
            vec![("x-opencode-session".to_string(), "sess-9".to_string())]
        );
    }

    #[test]
    fn debug_redacts_header_values() {
        let s = Settings::from_map(&vars(&[(
            "INFERENCE_HEADERS",
            "x-opencode-session: sess-secret",
        )]));
        let dbg = format!("{s:?}");
        assert!(dbg.contains("x-opencode-session"), "{dbg}");
        assert!(!dbg.contains("sess-secret"), "{dbg}");
    }

    #[test]
    fn now_secs_is_sane() {
        assert!(now_secs() > 1_700_000_000);
    }
}

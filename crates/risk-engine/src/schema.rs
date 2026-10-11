//! Prompt I/O schema (see skill `adaptive-risk-prompting`).
//!
//! [`TokenMetrics`] serializes to the skill's `INPUT PAYLOAD` and
//! [`RiskProfile`] deserializes from its `OUTPUT SCHEMA`. [`build_prompt`]
//! renders the `(system, user)` message pair sent to the model.

use serde::{Deserialize, Serialize};

/// Trade decision. Serializes exactly as the skill's `"BUY" | "PASS"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    /// Take the trade with the attached TP/SL parameters.
    #[serde(rename = "BUY")]
    Buy,
    /// Skip the trade.
    #[serde(rename = "PASS")]
    Pass,
}

/// Token snapshot fed to the model and to the heuristic fallback.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TokenMetrics {
    /// Liquidity in USD.
    pub liquidity_usd: f64,
    /// Buys observed in the last 5 seconds.
    pub buy_velocity_5s: u32,
    /// Deployer reputation in `0.0..=1.0`.
    pub dev_reputation: f64,
    /// RugCheck risk score.
    pub rugcheck_score: u32,
}

/// Model output: TP/SL percentages plus exit policy.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RiskProfile {
    /// Trade decision.
    pub action: Action,
    /// Take-profit target in percent (e.g. `75.0` = +75%).
    pub tp_percent: f64,
    /// Stop-loss trigger in percent (negative, e.g. `-8.0`).
    pub sl_percent: f64,
    /// Whether to trail the stop once in profit.
    pub trailing_stop: bool,
    /// Position exit timeout in seconds (`0` for [`Action::Pass`]).
    pub timeout_seconds: u64,
    /// Moonbag runner in percent (0–100): share left riding after TP.
    /// Skill `adaptive-risk-prompting`: 20 on high momentum, else 0.
    #[serde(default)]
    pub moonbag_percent: f64,
}

impl RiskProfile {
    /// Admission policy for model/LLM profiles (enforced by the engine
    /// before any profile reaches the ledger): TP in `(0, 1000%]`, SL in
    /// `[-100%, 0)`, timeout within an hour (`0` = no time-stop, used by
    /// moonbag runners). Unbounded magnitudes would create positions
    /// that never TP or never expire.
    pub fn is_valid(&self) -> bool {
        if !self.tp_percent.is_finite() || !self.sl_percent.is_finite() {
            return false;
        }
        if !self.moonbag_percent.is_finite()
            || self.moonbag_percent < 0.0
            || self.moonbag_percent > 100.0
        {
            return false;
        }
        match self.action {
            Action::Buy => {
                self.tp_percent > 0.0
                    && self.tp_percent <= MAX_TP_PERCENT
                    && self.sl_percent < 0.0
                    && self.sl_percent >= MIN_SL_PERCENT
                    && self.timeout_seconds <= MAX_TIMEOUT_SECS
            }
            Action::Pass => {
                self.tp_percent.abs() < 1e-9
                    && self.sl_percent.abs() < 1e-9
                    && self.timeout_seconds == 0
                    && !self.trailing_stop
                    && self.moonbag_percent == 0.0
            }
        }
    }
}

/// Maximum take-profit in percent (10x): beyond this a position would
/// effectively never take profit.
pub const MAX_TP_PERCENT: f64 = 1000.0;
/// Minimum stop-loss in percent (-100% = total loss): beyond this is
/// not a stop at all.
pub const MIN_SL_PERCENT: f64 = -100.0;
/// Maximum exit timeout in seconds (one hour): beyond this a position
/// would effectively never expire.
pub const MAX_TIMEOUT_SECS: u64 = 3600;

/// System instruction sent to the model (skill prompt specification
/// plus the decision bands, so the model follows the same table as the
/// heuristic fallback).
pub const SYSTEM_PROMPT: &str = "You are an ultra-fast quantitative risk engine for volatile Solana memecoins.\nEvaluate token metrics and return optimal Take-Profit (TP) and Stop-Loss (SL) percentages.\nOutput strictly valid JSON with no markdown formatting, preamble, or comments.\nRules: dev_reputation below 0.40 or rugcheck_score above 350 or buy_velocity_5s below 5 means PASS. High momentum (velocity above 25 with liquidity above $20000): TP 45 to 100, SL -10, trailing true, timeout 180, moonbag 20. Moderate momentum (velocity 5 to 25 with liquidity at least $10000): TP 30 to 50, SL -8, trailing false, timeout 90, moonbag 0. Otherwise (low liquidity or stagnant volume): PASS, never buy dead tokens. Bonding-curve spread and impact run 2 to 4 percent, so never set SL tighter than -8.";

impl TokenMetrics {
    /// Render the skill's `INPUT PAYLOAD` as a compact JSON string.
    pub fn to_input_payload(&self) -> String {
        serde_json::json!({
            "liquidity_usd": self.liquidity_usd,
            "buy_velocity_5s": self.buy_velocity_5s,
            "dev_reputation": self.dev_reputation,
            "rugcheck_score": self.rugcheck_score,
        })
        .to_string()
    }
}

/// Build the `(system, user)` message pair for the inference request.
pub fn build_prompt(metrics: &TokenMetrics) -> (String, String) {
    (SYSTEM_PROMPT.to_string(), metrics.to_input_payload())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_payload_matches_skill_shape() {
        let metrics = TokenMetrics {
            liquidity_usd: 30_000.0,
            buy_velocity_5s: 40,
            dev_reputation: 0.8,
            rugcheck_score: 120,
        };
        let value: serde_json::Value =
            serde_json::from_str(&metrics.to_input_payload()).expect("valid JSON");
        assert_eq!(value["liquidity_usd"], 30_000.0);
        assert_eq!(value["buy_velocity_5s"], 40);
        assert_eq!(value["dev_reputation"], 0.8);
        assert_eq!(value["rugcheck_score"], 120);
    }

    #[test]
    fn output_schema_roundtrip() {
        // Legacy model output without moonbag_percent still parses.
        let json = r#"{"action":"BUY","tp_percent":75.0,"sl_percent":-8.0,"trailing_stop":true,"timeout_seconds":180}"#;
        let profile: RiskProfile = serde_json::from_str(json).expect("valid JSON");
        assert_eq!(profile.action, Action::Buy);
        assert_eq!(profile.moonbag_percent, 0.0);
        assert!(profile.is_valid());
        assert_eq!(
            serde_json::to_value(profile).expect("serialize")["action"],
            "BUY"
        );
    }

    #[test]
    fn rejects_malformed_profiles() {
        let nan = RiskProfile {
            action: Action::Buy,
            tp_percent: f64::NAN,
            sl_percent: -8.0,
            trailing_stop: false,
            timeout_seconds: 90,
            moonbag_percent: 0.0,
        };
        assert!(!nan.is_valid());

        let inverted = RiskProfile {
            action: Action::Buy,
            tp_percent: -5.0,
            sl_percent: 3.0,
            trailing_stop: false,
            timeout_seconds: 90,
            moonbag_percent: 0.0,
        };
        assert!(!inverted.is_valid());

        let dirty_pass = RiskProfile {
            action: Action::Pass,
            tp_percent: 10.0,
            sl_percent: 0.0,
            trailing_stop: false,
            timeout_seconds: 0,
            moonbag_percent: 0.0,
        };
        assert!(!dirty_pass.is_valid());

        for bad_moonbag in [-1.0, 101.0, f64::NAN] {
            let bad = RiskProfile {
                action: Action::Buy,
                tp_percent: 60.0,
                sl_percent: -10.0,
                trailing_stop: true,
                timeout_seconds: 180,
                moonbag_percent: bad_moonbag,
            };
            assert!(!bad.is_valid(), "moonbag={bad_moonbag}");
        }

        // Zero-target BUY would fire on the first poll.
        let zero_buy = RiskProfile {
            action: Action::Buy,
            tp_percent: 0.0,
            sl_percent: 0.0,
            trailing_stop: false,
            timeout_seconds: 90,
            moonbag_percent: 0.0,
        };
        assert!(!zero_buy.is_valid());

        // Unbounded magnitudes would never TP or never expire.
        let huge_tp = RiskProfile {
            action: Action::Buy,
            tp_percent: 1e308,
            sl_percent: -8.0,
            trailing_stop: false,
            timeout_seconds: 90,
            moonbag_percent: 0.0,
        };
        assert!(!huge_tp.is_valid());
        let huge_sl = RiskProfile {
            action: Action::Buy,
            tp_percent: 60.0,
            sl_percent: -1e308,
            trailing_stop: false,
            timeout_seconds: 90,
            moonbag_percent: 0.0,
        };
        assert!(!huge_sl.is_valid());
        let huge_timeout = RiskProfile {
            action: Action::Buy,
            tp_percent: 60.0,
            sl_percent: -8.0,
            trailing_stop: false,
            timeout_seconds: u64::MAX,
            moonbag_percent: 0.0,
        };
        assert!(!huge_timeout.is_valid());

        // Boundary values are still admitted.
        let capped = RiskProfile {
            action: Action::Buy,
            tp_percent: MAX_TP_PERCENT,
            sl_percent: MIN_SL_PERCENT,
            trailing_stop: true,
            timeout_seconds: MAX_TIMEOUT_SECS,
            moonbag_percent: 100.0,
        };
        assert!(capped.is_valid());

        // Near-zero model outputs count as zeroed PASS targets.
        let dusty_pass = RiskProfile {
            action: Action::Pass,
            tp_percent: 1e-12,
            sl_percent: -1e-12,
            trailing_stop: false,
            timeout_seconds: 0,
            moonbag_percent: 0.0,
        };
        assert!(dusty_pass.is_valid());

        // Armed trailers on PASS are rejected.
        let armed_pass = RiskProfile {
            action: Action::Pass,
            tp_percent: 0.0,
            sl_percent: 0.0,
            trailing_stop: true,
            timeout_seconds: 0,
            moonbag_percent: 0.0,
        };
        assert!(!armed_pass.is_valid());
    }
}

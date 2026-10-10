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
}

impl RiskProfile {
    /// Structural validation: finite numbers, TP non-negative on BUY,
    /// SL non-positive on BUY, zeroed targets on PASS.
    pub fn is_valid(&self) -> bool {
        if !self.tp_percent.is_finite() || !self.sl_percent.is_finite() {
            return false;
        }
        match self.action {
            Action::Buy => self.tp_percent >= 0.0 && self.sl_percent <= 0.0,
            Action::Pass => {
                self.tp_percent == 0.0 && self.sl_percent == 0.0 && self.timeout_seconds == 0
            }
        }
    }
}

/// System instruction sent to the model (skill prompt specification).
pub const SYSTEM_PROMPT: &str = "You are an ultra-fast quantitative risk engine for volatile Solana memecoins.\nEvaluate token metrics and return optimal Take-Profit (TP) and Stop-Loss (SL) percentages.\nOutput strictly valid JSON with no markdown formatting, preamble, or comments.";

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
        let json = r#"{"action":"BUY","tp_percent":75.0,"sl_percent":-8.0,"trailing_stop":true,"timeout_seconds":180}"#;
        let profile: RiskProfile = serde_json::from_str(json).expect("valid JSON");
        assert_eq!(profile.action, Action::Buy);
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
        };
        assert!(!nan.is_valid());

        let inverted = RiskProfile {
            action: Action::Buy,
            tp_percent: -5.0,
            sl_percent: 3.0,
            trailing_stop: false,
            timeout_seconds: 90,
        };
        assert!(!inverted.is_valid());

        let dirty_pass = RiskProfile {
            action: Action::Pass,
            tp_percent: 10.0,
            sl_percent: 0.0,
            trailing_stop: false,
            timeout_seconds: 0,
        };
        assert!(!dirty_pass.is_valid());
    }
}

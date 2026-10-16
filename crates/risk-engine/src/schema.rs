//! Prompt I/O schema: input metrics, output profile.

use serde::{Deserialize, Serialize};

/// Trade decision: `"BUY" | "PASS"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    /// Take the trade with these TP/SL settings.
    #[serde(rename = "BUY")]
    Buy,
    /// Skip the trade.
    #[serde(rename = "PASS")]
    Pass,
}

/// Token snapshot for model and heuristic.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TokenMetrics {
    /// Liquidity in USD.
    pub liquidity_usd: f64,
    /// Buys in the last 5 seconds.
    pub buy_velocity_5s: u32,
    /// Deployer reputation in `0.0..=1.0`.
    pub dev_reputation: f64,
    /// RugCheck risk score.
    pub rugcheck_score: u32,
    /// Venue fee tier in bps per side (`None` = unknown, assume the
    /// standard 125bps tier). Carried so the model prices round-trip
    /// costs from a number instead of guessing the tier blind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub venue_fee_bps: Option<f64>,
    /// Bundle buys in the creation slot (`None` = unknown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creation_slot_buys: Option<u32>,
    /// Largest insider-cluster supply share in percent (`None` = unknown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster_pct: Option<f64>,
    /// Bonding-curve progress in percent (`None` = unknown, currently
    /// always unmeasured at confirm — no reserve field carries it; the
    /// prompt assumes the static 2–4% spread prior when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curve_progress: Option<f64>,
}

/// Model output: TP/SL plus exit policy.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RiskProfile {
    /// Trade decision.
    pub action: Action,
    /// Take-profit target in percent.
    pub tp_percent: f64,
    /// Stop-loss trigger in percent (negative).
    pub sl_percent: f64,
    /// Trail the stop once in profit.
    pub trailing_stop: bool,
    /// Exit timeout in seconds (`0` for [`Action::Pass`]).
    pub timeout_seconds: u64,
    /// Moonbag share left riding after TP, in percent.
    #[serde(default)]
    pub moonbag_percent: f64,
}

impl RiskProfile {
    /// Zeroed PASS profile (the only valid PASS shape).
    pub fn pass() -> Self {
        Self {
            action: Action::Pass,
            tp_percent: 0.0,
            sl_percent: 0.0,
            trailing_stop: false,
            timeout_seconds: 0,
            moonbag_percent: 0.0,
        }
    }

    /// Admission check for model profiles. Bounds are economic, not
    /// just syntactic: sub-fee TP/SL values are guaranteed losers after
    /// venue costs, so they fail closed to the heuristic instead.
    pub fn is_valid(&self) -> bool {
        if !self.tp_percent.is_finite() || !self.sl_percent.is_finite() {
            return false;
        }
        if !self.moonbag_percent.is_finite()
            || self.moonbag_percent < 0.0
            || self.moonbag_percent >= 100.0
        {
            return false;
        }
        match self.action {
            Action::Buy => {
                self.tp_percent >= MIN_TP_PERCENT
                    && self.tp_percent <= MAX_TP_PERCENT
                    && self.sl_percent <= -MIN_STOP_MAGNITUDE_PCT
                    && self.sl_percent >= MIN_SL_PERCENT
                    && self.timeout_seconds >= MIN_TIMEOUT_SECS
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

/// Max take-profit in percent (10x).
pub const MAX_TP_PERCENT: f64 = 1000.0;
/// Min take-profit in percent: below venue round-trip cost + margin,
/// so a first-uptick exit can't book a guaranteed loss after fees.
/// Well under the prompt's lowest band (TP 30), well above dust.
pub const MIN_TP_PERCENT: f64 = 10.0;
/// Min stop-loss in percent (-100% = full loss).
pub const MIN_SL_PERCENT: f64 = -100.0;
/// Min stop magnitude in percent: tighter stops drown in memecoin
/// noise. Matches the prompt's `-8` floor (2–4% spread/impact plus
/// round-trip venue costs); `fee_aware_sl_floor` widens further on
/// custom tiers, never tighter.
pub const MIN_STOP_MAGNITUDE_PCT: f64 = 8.0;
/// Max exit timeout in seconds (one hour).
pub const MAX_TIMEOUT_SECS: u64 = 3600;
/// Min exit timeout in seconds. A BUY with no timeout holds until
/// TP/SL forever, pinning a heat slot; only PASS is timeless.
pub const MIN_TIMEOUT_SECS: u64 = 1;

/// System instruction sent to the model.
pub const SYSTEM_PROMPT: &str = "You are an ultra-fast quantitative risk engine for volatile Solana memecoins.\nEvaluate token metrics and return optimal Take-Profit (TP) and Stop-Loss (SL) percentages.\nOutput strictly valid JSON with no markdown formatting, preamble, or comments.\nRules: dev_reputation below 0.40 or rugcheck_score above 350 or buy_velocity_5s below 5 means PASS. High momentum (velocity above 25 with liquidity above $20000): TP 45 to 100, SL -10, trailing true, timeout 180, moonbag 20. Moderate momentum (velocity 5 to 25 with liquidity at least $10000): TP 30 to 50, SL -8, trailing false, timeout 90, moonbag 0. Otherwise (low liquidity or stagnant volume): PASS, never buy dead tokens. Curve progress is currently unmeasured (curve_progress null): assume bonding-curve spread and impact run 2 to 4 percent, so never set SL tighter than -8. The payload carries venue_fee_bps when the fee tier is known (bps per side; absent means the standard 125bps tier). Round-trip venue cost is about 2*venue_fee_bps/100 percent: never set SL tighter than that cost plus margin, so on wide tiers (venue_fee_bps above 125) widen SL past it instead. High velocity with high creation_slot_buys or cluster_pct is suspect wash momentum: prefer PASS or the moderate band, never high-momentum TP. Hard bounds: BUY requires TP 10 to 1000, SL -100 to -8, timeout 1 to 3600, moonbag 0 to under 100; PASS must be all-zero with trailing false.";

impl TokenMetrics {
    /// Render the skill's `INPUT PAYLOAD` as a compact JSON string.
    pub fn to_input_payload(&self) -> String {
        serde_json::json!({
            "liquidity_usd": self.liquidity_usd,
            "buy_velocity_5s": self.buy_velocity_5s,
            "dev_reputation": self.dev_reputation,
            "rugcheck_score": self.rugcheck_score,
            "venue_fee_bps": self.venue_fee_bps,
            "creation_slot_buys": self.creation_slot_buys,
            "cluster_pct": self.cluster_pct,
            "curve_progress": self.curve_progress,
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
            venue_fee_bps: Some(125.0),
            creation_slot_buys: None,
            cluster_pct: None,
            curve_progress: None,
        };
        let value: serde_json::Value =
            serde_json::from_str(&metrics.to_input_payload()).expect("valid JSON");
        assert_eq!(value["liquidity_usd"], 30_000.0);
        assert_eq!(value["buy_velocity_5s"], 40);
        assert_eq!(value["dev_reputation"], 0.8);
        assert_eq!(value["rugcheck_score"], 120);
        assert_eq!(value["venue_fee_bps"], 125.0);
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

        // Sub-fee targets are guaranteed losers after venue costs.
        for (tp, sl) in [(5.0, -8.0), (60.0, -0.5), (9.9, -0.99)] {
            let dust = RiskProfile {
                action: Action::Buy,
                tp_percent: tp,
                sl_percent: sl,
                trailing_stop: false,
                timeout_seconds: 90,
                moonbag_percent: 0.0,
            };
            assert!(!dust.is_valid(), "tp={tp} sl={sl}");
        }
        // Floors themselves are admitted.
        let floored = RiskProfile {
            action: Action::Buy,
            tp_percent: MIN_TP_PERCENT,
            sl_percent: -MIN_STOP_MAGNITUDE_PCT,
            trailing_stop: false,
            timeout_seconds: 90,
            moonbag_percent: 0.0,
        };
        assert!(floored.is_valid());

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

        // Boundary values are still admitted (moonbag stays below 100%).
        let capped = RiskProfile {
            action: Action::Buy,
            tp_percent: MAX_TP_PERCENT,
            sl_percent: MIN_SL_PERCENT,
            trailing_stop: true,
            timeout_seconds: MAX_TIMEOUT_SECS,
            moonbag_percent: 99.0,
        };
        assert!(capped.is_valid());
        assert!(!RiskProfile { moonbag_percent: 100.0, ..capped }.is_valid());

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

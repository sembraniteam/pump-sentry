//! Deterministic heuristic fallback (see skill `adaptive-risk-prompting`).
//!
//! Runs in nanoseconds with no I/O. Mirrors the skill's decision table so
//! the pipeline keeps identical behavior when the model path is skipped.

use crate::schema::{Action, RiskProfile, TokenMetrics};

/// Skill thresholds, named for readability and testability.
pub const DEV_REPUTATION_FLOOR: f64 = 0.35;
/// Skill thresholds, named for readability and testability.
pub const RUGCHECK_SCORE_CAP: u32 = 350;
/// Skill thresholds, named for readability and testability.
pub const HIGH_MOMENTUM_VELOCITY: u32 = 30;
/// Skill thresholds, named for readability and testability.
pub const HIGH_MOMENTUM_LIQUIDITY_USD: f64 = 25_000.0;
/// Skill thresholds, named for readability and testability.
pub const MODERATE_MOMENTUM_VELOCITY: u32 = 10;
/// Live-data hardening (deliberate deviation from the skill, 2026-10-11):
/// with no evidence of demand the skill's scalp branch bought dead
/// tokens at a 0% win rate, so anything below this velocity is a PASS.
pub const MIN_BUY_VELOCITY: u32 = 5;

/// Instant heuristic evaluation.
///
/// Decision table (skill `adaptive-risk-prompting` as hardened by live
/// results — scalp TP/SL widened from 18/−4 because bonding-curve
/// spread+impact (~2–4%) stopped out −4% in milliseconds):
///
/// - `dev_reputation < 0.35`, `rugcheck_score > 350`, or
///   `velocity < 5` → PASS.
/// - High momentum (`velocity > 30`, `liq > $25k`) → TP 75, SL −8,
///   trailing, 180s.
/// - Moderate (`velocity >= 10`) → TP 30, SL −8, no trail, 90s.
/// - Otherwise (confirmed but thin demand) → TP 40, SL −10, no trail, 45s.
pub fn evaluate_heuristic_fallback(
    liq_usd: f64,
    buy_velocity_5s: u32,
    dev_rep: f64,
    rugcheck_score: u32,
) -> RiskProfile {
    if dev_rep < DEV_REPUTATION_FLOOR
        || rugcheck_score > RUGCHECK_SCORE_CAP
        || buy_velocity_5s < MIN_BUY_VELOCITY
    {
        return RiskProfile {
            action: Action::Pass,
            tp_percent: 0.0,
            sl_percent: 0.0,
            trailing_stop: false,
            timeout_seconds: 0,
        };
    }

    if buy_velocity_5s > HIGH_MOMENTUM_VELOCITY && liq_usd > HIGH_MOMENTUM_LIQUIDITY_USD {
        RiskProfile {
            action: Action::Buy,
            tp_percent: 75.0,
            sl_percent: -8.0,
            trailing_stop: true,
            timeout_seconds: 180,
        }
    } else if buy_velocity_5s >= MODERATE_MOMENTUM_VELOCITY {
        RiskProfile {
            action: Action::Buy,
            tp_percent: 30.0,
            sl_percent: -8.0,
            trailing_stop: false,
            timeout_seconds: 90,
        }
    } else {
        RiskProfile {
            action: Action::Buy,
            tp_percent: 40.0,
            sl_percent: -10.0,
            trailing_stop: false,
            timeout_seconds: 45,
        }
    }
}

/// [`TokenMetrics`] convenience wrapper over [`evaluate_heuristic_fallback`].
pub fn evaluate_metrics_fallback(metrics: &TokenMetrics) -> RiskProfile {
    evaluate_heuristic_fallback(
        metrics.liquidity_usd,
        metrics.buy_velocity_5s,
        metrics.dev_reputation,
        metrics.rugcheck_score,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_on_bad_dev() {
        let p = evaluate_heuristic_fallback(100_000.0, 100, 0.34, 0);
        assert_eq!(p.action, Action::Pass);
        assert_eq!(p.timeout_seconds, 0);
    }

    #[test]
    fn passes_on_high_rugcheck_score() {
        let p = evaluate_heuristic_fallback(100_000.0, 100, 0.9, 351);
        assert_eq!(p.action, Action::Pass);
    }

    #[test]
    fn high_momentum_profile() {
        let p = evaluate_heuristic_fallback(25_001.0, 31, 0.9, 100);
        assert_eq!(p.action, Action::Buy);
        assert_eq!(p.tp_percent, 75.0);
        assert_eq!(p.sl_percent, -8.0);
        assert!(p.trailing_stop);
        assert_eq!(p.timeout_seconds, 180);
    }

    #[test]
    fn boundary_velocity_30_is_moderate() {
        let p = evaluate_heuristic_fallback(100_000.0, 30, 0.9, 100);
        assert_eq!((p.tp_percent, p.timeout_seconds), (30.0, 90));
    }

    #[test]
    fn boundary_velocity_10_is_moderate() {
        let p = evaluate_heuristic_fallback(1_000.0, 10, 0.9, 100);
        assert_eq!((p.tp_percent, p.sl_percent), (30.0, -8.0));
    }

    #[test]
    fn no_demand_is_pass() {
        // The 0%-win-rate killer: stubbed/zero velocity must never buy.
        for v in [0, 1, 4] {
            let p = evaluate_heuristic_fallback(5_000.0, v, 0.9, 100);
            assert_eq!(p.action, Action::Pass, "velocity={v}");
            assert_eq!(p.timeout_seconds, 0);
        }
    }

    #[test]
    fn thin_demand_buys_wide() {
        let p = evaluate_heuristic_fallback(5_000.0, 9, 0.9, 100);
        assert_eq!(
            (p.tp_percent, p.sl_percent, p.timeout_seconds),
            (40.0, -10.0, 45)
        );
        assert!(!p.trailing_stop);
    }

    #[test]
    fn boundary_velocity_5_is_thin_buy() {
        let p = evaluate_heuristic_fallback(5_000.0, 5, 0.9, 100);
        assert_eq!(p.action, Action::Buy);
        assert_eq!((p.tp_percent, p.sl_percent), (40.0, -10.0));
    }

    #[test]
    fn all_fallback_profiles_are_valid() {
        for v in [0, 9, 10, 30, 31, 100] {
            for liq in [0.0, 9_999.0, 25_000.0, 25_001.0, 1_000_000.0] {
                let p = evaluate_heuristic_fallback(liq, v, 0.9, 100);
                assert!(p.is_valid(), "invalid for v={v} liq={liq}: {p:?}");
            }
        }
    }
}

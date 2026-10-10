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

/// Instant heuristic evaluation, exactly per the skill's decision table:
///
/// - `dev_reputation < 0.35` or `rugcheck_score > 350` → PASS.
/// - High momentum (`velocity > 30`, `liq > $25k`) → TP 75, SL −8,
///   trailing, 180s.
/// - Moderate (`velocity >= 10`) → TP 30, SL −6, no trail, 90s.
/// - Otherwise (low-liquidity scalp) → TP 18, SL −4, no trail, 45s.
pub fn evaluate_heuristic_fallback(
    liq_usd: f64,
    buy_velocity_5s: u32,
    dev_rep: f64,
    rugcheck_score: u32,
) -> RiskProfile {
    if dev_rep < DEV_REPUTATION_FLOOR || rugcheck_score > RUGCHECK_SCORE_CAP {
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
            sl_percent: -6.0,
            trailing_stop: false,
            timeout_seconds: 90,
        }
    } else {
        RiskProfile {
            action: Action::Buy,
            tp_percent: 18.0,
            sl_percent: -4.0,
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
        assert_eq!((p.tp_percent, p.sl_percent), (30.0, -6.0));
    }

    #[test]
    fn low_velocity_is_scalp() {
        let p = evaluate_heuristic_fallback(5_000.0, 9, 0.9, 100);
        assert_eq!(
            (p.tp_percent, p.sl_percent, p.timeout_seconds),
            (18.0, -4.0, 45)
        );
        assert!(!p.trailing_stop);
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

//! Deterministic heuristic fallback (see skill `adaptive-risk-prompting`).
//!
//! Runs in nanoseconds with no I/O. Mirrors the skill's decision table so
//! the pipeline keeps identical behavior when the model path is skipped.

use crate::schema::{Action, RiskProfile, TokenMetrics};

/// Skill thresholds, named for readability and testability.
pub const DEV_REPUTATION_FLOOR: f64 = 0.40;
/// Skill thresholds, named for readability and testability.
///
/// `RUGCHECK_SCORE_CAP` (350) vs the auditor's hard gate (400 in
/// `auditor/src/policy.rs::AuditPolicy::default`): this is the softer
/// pre-LLM triage — scores 351–400 are still *observed* but resolve to
/// PASS here, so only the model path (or a threshold change) can buy
/// them. Keep `RUGCHECK_SCORE_CAP <= AuditPolicy::max_score`.
pub const RUGCHECK_SCORE_CAP: u32 = 350;
/// Skill thresholds, named for readability and testability.
pub const HIGH_MOMENTUM_VELOCITY: u32 = 25;
/// Skill thresholds, named for readability and testability.
pub const HIGH_MOMENTUM_LIQUIDITY_USD: f64 = 20_000.0;
/// Skill thresholds, named for readability and testability.
pub const MODERATE_MOMENTUM_VELOCITY: u32 = 5;
/// Skill thresholds, named for readability and testability.
pub const MODERATE_MOMENTUM_LIQUIDITY_USD: f64 = 10_000.0;

/// Instant heuristic evaluation, exactly per the skill's decision table
/// (hardened 2026-10-11: never buy dead tokens):
///
/// - `dev_reputation < 0.40`, `rugcheck_score > 350`, or
///   `velocity < 5` → PASS.
/// - High momentum (`velocity > 25`, `liq > $20k`) → TP 60, SL −10,
///   trailing, 180s.
/// - Moderate (`velocity >= 5`, `liq >= $10k`) → TP 35, SL −8, no trail, 90s.
/// - Otherwise (unproven liquidity/velocity) → PASS.
///
/// PASS shape (zeroed targets, disarmed trailing/moonbag) is shared so
/// the three call sites cannot drift apart.
const fn pass_profile() -> RiskProfile {
    RiskProfile {
        action: Action::Pass,
        tp_percent: 0.0,
        sl_percent: 0.0,
        trailing_stop: false,
        timeout_seconds: 0,
        moonbag_percent: 0.0,
    }
}
pub fn evaluate_heuristic_fallback(
    liq_usd: f64,
    buy_velocity_5s: u32,
    dev_rep: f64,
    rugcheck_score: u32,
) -> RiskProfile {
    // NaN comparisons are all false (`NaN < x == false`), which would
    // sail a non-reputation straight into the momentum branches.
    // Fail closed on any non-finite input.
    if !liq_usd.is_finite() || !dev_rep.is_finite() {
        return pass_profile();
    }
    if dev_rep < DEV_REPUTATION_FLOOR
        || rugcheck_score > RUGCHECK_SCORE_CAP
        || buy_velocity_5s < MODERATE_MOMENTUM_VELOCITY
    {
        return pass_profile();
    }
    if buy_velocity_5s > HIGH_MOMENTUM_VELOCITY && liq_usd > HIGH_MOMENTUM_LIQUIDITY_USD {
        RiskProfile {
            action: Action::Buy,
            tp_percent: 60.0,
            sl_percent: -10.0,
            trailing_stop: true,
            timeout_seconds: 180,
            moonbag_percent: 20.0,
        }
    } else if liq_usd >= MODERATE_MOMENTUM_LIQUIDITY_USD {
        RiskProfile {
            action: Action::Buy,
            tp_percent: 35.0,
            sl_percent: -8.0,
            trailing_stop: false,
            timeout_seconds: 90,
            moonbag_percent: 0.0,
        }
    } else {
        pass_profile()
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
    fn rejects_nan_inputs_fail_closed() {
        for (liq, dev) in [(f64::NAN, 0.9), (10_000.0, f64::NAN), (f64::NAN, f64::NAN)] {
            let p = evaluate_heuristic_fallback(liq, 100, dev, 0);
            assert_eq!(p.action, Action::Pass, "liq={liq} dev={dev}");
        }
    }

    #[test]
    fn passes_on_bad_dev() {
        let p = evaluate_heuristic_fallback(100_000.0, 100, 0.39, 0);
        assert_eq!(p.action, Action::Pass);
        assert_eq!(p.timeout_seconds, 0);
    }

    #[test]
    fn passes_on_high_rugcheck_score() {
        let p = evaluate_heuristic_fallback(100_000.0, 100, 0.9, 351);
        assert_eq!(p.action, Action::Pass);
    }

    #[test]
    fn prompt_bands_match_heuristic_constants() {
        // SYSTEM_PROMPT duplicates the decision bands as prose; a
        // constant change without a prompt change silently forks the
        // model policy from the heuristic. This test forces them to
        // move together.
        let prompt = crate::schema::SYSTEM_PROMPT;
        assert!(prompt.contains(&DEV_REPUTATION_FLOOR.to_string()), "dev floor");
        assert!(prompt.contains(&RUGCHECK_SCORE_CAP.to_string()), "score cap");
        assert!(prompt.contains(&HIGH_MOMENTUM_VELOCITY.to_string()), "high velocity");
        assert!(
            prompt.contains(&HIGH_MOMENTUM_LIQUIDITY_USD.to_string()),
            "high liquidity"
        );
        assert!(
            prompt.contains(&MODERATE_MOMENTUM_VELOCITY.to_string()),
            "moderate velocity"
        );
        assert!(
            prompt.contains(&MODERATE_MOMENTUM_LIQUIDITY_USD.to_string()),
            "moderate liquidity"
        );
    }

    #[test]
    fn high_momentum_profile() {
        let p = evaluate_heuristic_fallback(20_001.0, 26, 0.9, 100);
        assert_eq!(p.action, Action::Buy);
        assert_eq!(p.tp_percent, 60.0);
        assert_eq!(p.sl_percent, -10.0);
        assert!(p.trailing_stop);
        assert_eq!(p.timeout_seconds, 180);
        assert_eq!(p.moonbag_percent, 20.0);
    }

    #[test]
    fn boundary_velocity_25_is_moderate() {
        let p = evaluate_heuristic_fallback(100_000.0, 25, 0.9, 100);
        assert_eq!((p.tp_percent, p.timeout_seconds), (35.0, 90));
    }

    #[test]
    fn boundary_velocity_5_is_moderate() {
        let p = evaluate_heuristic_fallback(10_000.0, 5, 0.9, 100);
        assert_eq!((p.tp_percent, p.sl_percent), (35.0, -8.0));
        assert_eq!(p.moonbag_percent, 0.0);
    }

    #[test]
    fn thin_liquidity_is_pass() {
        // Proven velocity but unproven liquidity: never buy.
        let p = evaluate_heuristic_fallback(9_999.0, 20, 0.9, 100);
        assert_eq!(p.action, Action::Pass);
        assert_eq!(p.timeout_seconds, 0);
    }

    #[test]
    fn no_demand_is_pass() {
        // The 0%-win-rate killer: stubbed/zero velocity must never buy.
        for v in [0, 1, 4] {
            let p = evaluate_heuristic_fallback(50_000.0, v, 0.9, 100);
            assert_eq!(p.action, Action::Pass, "velocity={v}");
            assert_eq!(p.timeout_seconds, 0);
        }
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

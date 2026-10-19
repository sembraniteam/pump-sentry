//! Deterministic heuristic fallback.

use crate::schema::{Action, RiskProfile, TokenMetrics};

/// Dev reputation floor.
pub const DEV_REPUTATION_FLOOR: f64 = 0.40;
/// Pre-LLM score cap. Keep `<= AuditPolicy::max_score` (400).
/// Internal heuristic (see the policy cap note); the model-side PASS
/// rule above 350 is advisory, enforced by `apply_score_gate`.
pub const RUGCHECK_SCORE_CAP: u32 = 350;
/// High-momentum buy velocity.
pub const HIGH_MOMENTUM_VELOCITY: u32 = 25;
/// High-momentum liquidity in USD.
pub const HIGH_MOMENTUM_LIQUIDITY_USD: f64 = 20_000.0;
/// Moderate-momentum buy velocity.
pub const MODERATE_MOMENTUM_VELOCITY: u32 = 5;
/// Moderate-momentum liquidity in USD.
pub const MODERATE_MOMENTUM_LIQUIDITY_USD: f64 = 10_000.0;
/// Wash-suspect creation-slot buys. Pinned to the bundle slot rule:
/// 3+ creation-slot buys means bundled (see `listener::bundle`).
pub const WASH_CREATION_SLOT_BUYS: u32 = 3;
/// Wash-suspect cluster share, in percent. Pinned to the monopoly
/// line: over 15% rejects (see `listener::bundle`).
pub const WASH_CLUSTER_PCT: f64 = 15.0;

/// Heuristic decision. Mirrors the model table.
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
/// Heuristic decision. Mirrors the model table, including the wash
/// guidance: high-band momentum on wash-suspect flow drops to the
/// moderate band (never high-momentum TP). Unknown forensics
/// (`None`) prove nothing and change nothing — the bundle gate
/// already drops Reject and paper-gates Suspect upstream.
pub fn evaluate_heuristic_fallback(metrics: &TokenMetrics) -> RiskProfile {
    let liq_usd = metrics.liquidity_usd;
    let buy_velocity_5s = metrics.buy_velocity_5s;
    let dev_rep = metrics.dev_reputation;
    let rugcheck_score = metrics.rugcheck_score;
    // NaN never compares true. Fail closed.
    if !liq_usd.is_finite() || !dev_rep.is_finite() {
        return pass_profile();
    }
    if dev_rep < DEV_REPUTATION_FLOOR
        || rugcheck_score > RUGCHECK_SCORE_CAP
        || buy_velocity_5s < MODERATE_MOMENTUM_VELOCITY
    {
        return pass_profile();
    }
    let high_momentum =
        buy_velocity_5s > HIGH_MOMENTUM_VELOCITY && liq_usd > HIGH_MOMENTUM_LIQUIDITY_USD;
    if high_momentum && is_wash_suspect(metrics) {
        return moderate_profile();
    }
    if high_momentum {
        high_profile()
    } else if buy_velocity_5s >= MODERATE_MOMENTUM_VELOCITY
        && liq_usd >= MODERATE_MOMENTUM_LIQUIDITY_USD
    {
        moderate_profile()
    } else {
        pass_profile()
    }
}

/// High-momentum band profile.
const fn high_profile() -> RiskProfile {
    RiskProfile {
        action: Action::Buy,
        tp_percent: 60.0,
        sl_percent: -10.0,
        trailing_stop: true,
        timeout_seconds: 180,
        moonbag_percent: 20.0,
    }
}

/// Moderate-momentum band profile.
const fn moderate_profile() -> RiskProfile {
    RiskProfile {
        action: Action::Buy,
        tp_percent: 35.0,
        sl_percent: -8.0,
        trailing_stop: false,
        timeout_seconds: 90,
        moonbag_percent: 0.0,
    }
}

/// Wash-suspect flow: bundled creation slot or monopoly-scale
/// cluster. Thresholds mirror the bundle detector (slot rule,
/// monopoly line); `None` is unknown, not evidence.
fn is_wash_suspect(metrics: &TokenMetrics) -> bool {
    metrics
        .creation_slot_buys
        .is_some_and(|b| b >= WASH_CREATION_SLOT_BUYS)
        || metrics.cluster_pct.is_some_and(|c| c > WASH_CLUSTER_PCT)
}

/// [`TokenMetrics`] wrapper.
pub fn evaluate_metrics_fallback(metrics: &TokenMetrics) -> RiskProfile {
    evaluate_heuristic_fallback(metrics)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(liq: f64, vel: u32, dev: f64, score: u32) -> TokenMetrics {
        TokenMetrics {
            liquidity_usd: liq,
            buy_velocity_5s: vel,
            dev_reputation: dev,
            rugcheck_score: score,
            venue_fee_bps: None,
            creation_slot_buys: None,
            cluster_pct: None,
            curve_progress: None,
            volume_usd: None,
            buy_ratio: None,
            unique_traders: 0,
            top_wallet_share_pct: None,
            creator_net_base: 0,
            velocity_delta: None,
            top_holder_pct: None,
        }
    }

    #[test]
    fn rejects_nan_inputs_fail_closed() {
        for (liq, dev) in [(f64::NAN, 0.9), (10_000.0, f64::NAN), (f64::NAN, f64::NAN)] {
            let p = evaluate_heuristic_fallback(&m(liq, 100, dev, 0));
            assert_eq!(p.action, Action::Pass, "liq={liq} dev={dev}");
        }
    }

    #[test]
    fn passes_on_bad_dev() {
        let p = evaluate_heuristic_fallback(&m(100_000.0, 100, 0.39, 0));
        assert_eq!(p.action, Action::Pass);
        assert_eq!(p.timeout_seconds, 0);
    }

    #[test]
    fn passes_on_high_rugcheck_score() {
        let p = evaluate_heuristic_fallback(&m(100_000.0, 100, 0.9, 351));
        assert_eq!(p.action, Action::Pass);
    }

    #[test]
    fn prompt_bands_match_heuristic_constants() {
        // SYSTEM_PROMPT duplicates the bands; constants and prompt move together.
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
        // Bundle-forensics and hard-bounds sentences pin with them.
        assert!(prompt.contains("wash momentum"), "wash guidance");
        assert!(prompt.contains("all-zero"), "pass shape");
    }

    #[test]
    fn high_momentum_profile() {
        let p = evaluate_heuristic_fallback(&m(20_001.0, 26, 0.9, 100));
        assert_eq!(p.action, Action::Buy);
        assert_eq!(p.tp_percent, 60.0);
        assert_eq!(p.sl_percent, -10.0);
        assert!(p.trailing_stop);
        assert_eq!(p.timeout_seconds, 180);
        assert_eq!(p.moonbag_percent, 20.0);
    }

    #[test]
    fn boundary_velocity_25_is_moderate() {
        let p = evaluate_heuristic_fallback(&m(100_000.0, 25, 0.9, 100));
        assert_eq!((p.tp_percent, p.timeout_seconds), (35.0, 90));
    }

    #[test]
    fn boundary_velocity_5_is_moderate() {
        let p = evaluate_heuristic_fallback(&m(10_000.0, 5, 0.9, 100));
        assert_eq!((p.tp_percent, p.sl_percent), (35.0, -8.0));
        assert_eq!(p.moonbag_percent, 0.0);
    }

    #[test]
    fn thin_liquidity_is_pass() {
        let p = evaluate_heuristic_fallback(&m(9_999.0, 20, 0.9, 100));
        assert_eq!(p.action, Action::Pass);
        assert_eq!(p.timeout_seconds, 0);
    }

    #[test]
    fn no_demand_is_pass() {
        for v in [0, 1, 4] {
            let p = evaluate_heuristic_fallback(&m(50_000.0, v, 0.9, 100));
            assert_eq!(p.action, Action::Pass, "velocity={v}");
            assert_eq!(p.timeout_seconds, 0);
        }
    }

    #[test]
    fn all_fallback_profiles_are_valid() {
        for v in [0, 9, 10, 30, 31, 100] {
            for liq in [0.0, 9_999.0, 25_000.0, 25_001.0, 1_000_000.0] {
                let p = evaluate_heuristic_fallback(&m(liq, v, 0.9, 100));
                assert!(p.is_valid(), "invalid for v={v} liq={liq}: {p:?}");
            }
        }
    }

    #[test]
    fn subfloor_dev_reputation_passes() {
        // Reachable again now that the scorer no longer clamps at
        // 0.40: heavy prior-launch penalties trip the floor rule.
        for dev in [0.39, 0.2, 0.0] {
            let p = evaluate_heuristic_fallback(&m(100_000.0, 100, dev, 100));
            assert_eq!(p.action, Action::Pass, "dev={dev}");
        }
    }

    #[test]
    fn wash_flow_never_takes_high_band() {
        // 3+ creation-slot buys: bundled by the slot rule.
        let mut w = m(20_001.0, 26, 0.9, 100);
        w.creation_slot_buys = Some(3);
        let p = evaluate_heuristic_fallback(&w);
        assert_eq!(p.action, Action::Buy);
        assert_eq!((p.tp_percent, p.sl_percent), (35.0, -8.0));
        assert!(!p.trailing_stop);
        assert_eq!((p.timeout_seconds, p.moonbag_percent), (90, 0.0));
        assert!(p.is_valid());
        // Monopoly-scale cluster: same downgrade.
        let mut c = m(20_001.0, 26, 0.9, 100);
        c.cluster_pct = Some(15.1);
        assert_eq!(evaluate_heuristic_fallback(&c).tp_percent, 35.0);
        // Boundaries stay high: 2 buys, exactly 15%.
        let mut b = m(20_001.0, 26, 0.9, 100);
        b.creation_slot_buys = Some(2);
        b.cluster_pct = Some(15.0);
        assert_eq!(evaluate_heuristic_fallback(&b).tp_percent, 60.0);
        // Unknown forensics change nothing.
        assert_eq!(
            evaluate_heuristic_fallback(&m(20_001.0, 26, 0.9, 100)).tp_percent,
            60.0
        );
        // Wash signals outside the high band change nothing either.
        let mut low = m(100_000.0, 10, 0.9, 100);
        low.creation_slot_buys = Some(9);
        low.cluster_pct = Some(90.0);
        assert_eq!(evaluate_heuristic_fallback(&low).tp_percent, 35.0);
    }
}

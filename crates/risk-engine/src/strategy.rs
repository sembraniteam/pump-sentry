//! Execution strategies: how a BUY turns into exits.

use crate::schema::{Action, RiskProfile};
use std::str::FromStr;

/// Scalp TP floor in percent. Warn-only (see the confirm gate):
/// below-band TPs are honored as early exits, never widened.
pub const SCALP_TP_MIN: f64 = 25.0;
/// Scalp TP cap in percent.
pub const SCALP_TP_MAX: f64 = 35.0;
/// Runner default: sell 80% at TP, ride 20%.
pub const RUNNER_MOONBAG_DEFAULT: f64 = 20.0;

/// Execution strategy selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StrategyMode {
    /// Follow the profile as-is.
    #[default]
    Adaptive,
    /// Quick scalp: clamp TP down, full exit, no runner.
    ScalpQuick,
    /// Moonbag runner: sell most at TP, ride the rest.
    Runner,
}

impl FromStr for StrategyMode {
    type Err = String;

    /// Parses `adaptive` | `scalp` | `runner` (case-insensitive).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "adaptive" => Ok(StrategyMode::Adaptive),
            "scalp" => Ok(StrategyMode::ScalpQuick),
            "runner" => Ok(StrategyMode::Runner),
            other => Err(format!("unknown strategy mode '{other}'")),
        }
    }
}

/// Strategy knobs (built from settings).
#[derive(Debug, Clone, Copy)]
pub struct StrategyConfig {
    /// Execution shape selector.
    pub mode: StrategyMode,
    /// Runner share kept riding after TP.
    pub runner_moonbag_percent: f64,
}

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            mode: StrategyMode::Adaptive,
            runner_moonbag_percent: RUNNER_MOONBAG_DEFAULT,
        }
    }
}

/// Rewrite `profile` per `cfg`. Pure.
/// PASS and invalid profiles return unchanged.
pub fn resolve_strategy(profile: &RiskProfile, cfg: &StrategyConfig) -> RiskProfile {
    if profile.action != Action::Buy || !profile.is_valid() {
        return *profile;
    }
    let mut resolved = match cfg.mode {
        StrategyMode::Adaptive => *profile,
        StrategyMode::ScalpQuick => RiskProfile {
            action: Action::Buy,
            tp_percent: profile.tp_percent.min(SCALP_TP_MAX),
            sl_percent: profile.sl_percent,
            trailing_stop: profile.trailing_stop,
            timeout_seconds: profile.timeout_seconds,
            moonbag_percent: 0.0,
        },
        StrategyMode::Runner => RiskProfile {
            action: Action::Buy,
            tp_percent: profile.tp_percent,
            sl_percent: profile.sl_percent,
            trailing_stop: true,
            timeout_seconds: profile.timeout_seconds,
            // `clamp` on NaN stays NaN. Clean first.
            moonbag_percent: {
                let moonbag = finite_or(cfg.runner_moonbag_percent, RUNNER_MOONBAG_DEFAULT)
                    .clamp(0.0, 100.0);
                // 100% moonbag sells nothing. Use full exit.
                if moonbag >= 100.0 { 0.0 } else { moonbag }
            },
        },
    };
    // Moonbag without trail never exits sideways. Force it on.
    if resolved.moonbag_percent > 0.0 {
        resolved.trailing_stop = true;
    }
    resolved
}

/// Finite `v`, else `default`.
fn finite_or(v: f64, default: f64) -> f64 {
    if v.is_finite() { v } else { default }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buy(tp: f64, moonbag: f64, trailing: bool) -> RiskProfile {
        RiskProfile {
            action: Action::Buy,
            tp_percent: tp,
            sl_percent: -8.0,
            trailing_stop: trailing,
            timeout_seconds: 90,
            moonbag_percent: moonbag,
        }
    }

    #[test]
    fn mode_parses_case_insensitively() {
        assert_eq!("adaptive".parse(), Ok(StrategyMode::Adaptive));
        assert_eq!("SCALP".parse(), Ok(StrategyMode::ScalpQuick));
        assert_eq!("Runner".parse(), Ok(StrategyMode::Runner));
        assert!("yolo".parse::<StrategyMode>().is_err());
        // Removed aliases stay removed (fail-closed to adaptive).
        assert!("MoonBag".parse::<StrategyMode>().is_err());
        assert!("scalp_quick".parse::<StrategyMode>().is_err());
    }

    #[test]
    fn adaptive_is_passthrough() {
        let p = buy(60.0, 20.0, true);
        assert_eq!(resolve_strategy(&p, &StrategyConfig::default()), p);
    }

    #[test]
    fn moonbag_without_trailing_gets_a_trail() {
        let cfg = StrategyConfig::default();
        let p = resolve_strategy(&buy(60.0, 20.0, false), &cfg);
        assert!(p.trailing_stop);
        assert_eq!(p.moonbag_percent, 20.0);
    }

    #[test]
    fn pass_never_rewritten() {
        let pass = RiskProfile {
            action: Action::Pass,
            tp_percent: 0.0,
            sl_percent: 0.0,
            trailing_stop: false,
            timeout_seconds: 0,
            moonbag_percent: 0.0,
        };
        for mode in [
            StrategyMode::Adaptive,
            StrategyMode::ScalpQuick,
            StrategyMode::Runner,
        ] {
            let cfg = StrategyConfig {
                mode,
                ..StrategyConfig::default()
            };
            assert_eq!(resolve_strategy(&pass, &cfg), pass);
        }
    }

    #[test]
    fn scalp_clamps_down_and_kills_moonbag() {
        let cfg = StrategyConfig {
            mode: StrategyMode::ScalpQuick,
            ..StrategyConfig::default()
        };
        let high = resolve_strategy(&buy(90.0, 20.0, true), &cfg);
        assert_eq!((high.tp_percent, high.moonbag_percent), (35.0, 0.0));
        assert!(high.trailing_stop);
        // Below the band passes through untouched (caller must warn).
        let low = resolve_strategy(&buy(10.0, 0.0, false), &cfg);
        assert_eq!((low.tp_percent, low.moonbag_percent), (10.0, 0.0));
        let mid = resolve_strategy(&buy(30.0, 0.0, false), &cfg);
        assert_eq!(mid.tp_percent, 30.0);
        assert!(mid.is_valid());
    }

    #[test]
    fn invalid_profiles_pass_through_untouched() {
        let invalid = RiskProfile {
            action: Action::Buy,
            tp_percent: -5.0,
            sl_percent: 3.0,
            trailing_stop: false,
            timeout_seconds: 90,
            moonbag_percent: 0.0,
        };
        assert!(!invalid.is_valid());
        for mode in [
            StrategyMode::Adaptive,
            StrategyMode::ScalpQuick,
            StrategyMode::Runner,
        ] {
            let cfg = StrategyConfig {
                mode,
                ..StrategyConfig::default()
            };
            assert_eq!(resolve_strategy(&invalid, &cfg), invalid);
        }
    }

    #[test]
    fn nan_moonbag_config_falls_back_to_default() {
        let cfg = StrategyConfig {
            mode: StrategyMode::Runner,
            runner_moonbag_percent: f64::NAN,
        };
        let p = resolve_strategy(&buy(60.0, 0.0, false), &cfg);
        assert_eq!(p.moonbag_percent, RUNNER_MOONBAG_DEFAULT);
        assert!(p.is_valid());
    }

    #[test]
    fn runner_forces_moonbag_and_trailing() {
        let cfg = StrategyConfig {
            mode: StrategyMode::Runner,
            runner_moonbag_percent: 20.0,
        };
        let p = resolve_strategy(&buy(60.0, 0.0, false), &cfg);
        assert_eq!((p.tp_percent, p.moonbag_percent), (60.0, 20.0));
        assert!(p.trailing_stop);
        assert!(p.is_valid());
        // Custom moonbag share honored, clamped, never 100%.
        let custom = StrategyConfig {
            mode: StrategyMode::Runner,
            runner_moonbag_percent: 150.0,
        };
        assert_eq!(
            resolve_strategy(&buy(60.0, 0.0, false), &custom).moonbag_percent,
            0.0
        );
    }
}

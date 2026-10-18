//! Validated money-domain units (config side, disjoint from dispatch DTOs).
//!
//! `live.rs` owns dispatch DTOs (other owner — do not touch); this module
//! owns validated config units so `settings.rs` can adopt them later
//! without crossing that boundary.
//!
//! TODO(settings): migrate `Settings::{fee_bps, slippage_bps,
//! trail_percent, runner_trail_percent, runner_moonbag_percent}` to
//! [`Bps`]/[`Percent`] once the owning agents agree on the boundary.
//! For now these types are provided + tested; `Settings` keeps its
//! primitive fields so behavior is unchanged.

/// Basis points, validated `0..=10_000` (100% = 10_000).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bps(u32);

impl Bps {
    /// Denominator: 100% in basis points. Single source is
    /// `ids::BPS_DENOMINATOR`.
    pub const DENOMINATOR: u32 = ids::BPS_DENOMINATOR as u32;

    /// Validated constructor: `None` when above 100%.
    #[must_use]
    pub const fn new(v: u32) -> Option<Self> {
        if v <= Self::DENOMINATOR {
            Some(Self(v))
        } else {
            None
        }
    }

    /// Raw value in basis points.
    #[allow(dead_code)]
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// True at exactly 100%.
    #[must_use]
    pub const fn is_full(self) -> bool {
        self.0 == Self::DENOMINATOR
    }
}

/// Percent, validated finite `0.0..=100.0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Percent(f64);

impl Percent {
    /// Validated constructor: `None` for NaN/inf or outside `0–100`.
    #[must_use]
    pub fn new(v: f64) -> Option<Self> {
        if v.is_finite() && (0.0..=100.0).contains(&v) {
            Some(Self(v))
        } else {
            None
        }
    }

    /// Raw value in percent.
    #[allow(dead_code)]
    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bps_accepts_range_rejects_over() {
        assert_eq!(Bps::new(0).map(Bps::get), Some(0));
        assert_eq!(Bps::new(200).map(Bps::get), Some(200));
        assert_eq!(Bps::new(10_000).map(Bps::get), Some(10_000));
        assert!(Bps::new(10_001).is_none());
        assert!(Bps::new(u32::MAX).is_none());
        assert!(Bps::new(10_000).is_some_and(|b| b.is_full()));
        assert!(!Bps::new(0).is_some_and(|b| b.is_full()));
    }

    #[test]
    fn percent_accepts_range_rejects_garbage() {
        assert_eq!(Percent::new(0.0).map(Percent::get), Some(0.0));
        assert_eq!(Percent::new(20.0).map(Percent::get), Some(20.0));
        assert_eq!(Percent::new(100.0).map(Percent::get), Some(100.0));
        assert!(Percent::new(-0.1).is_none());
        assert!(Percent::new(100.1).is_none());
        assert!(Percent::new(f64::NAN).is_none());
        assert!(Percent::new(f64::INFINITY).is_none());
        assert!(Percent::new(f64::NEG_INFINITY).is_none());
    }
}

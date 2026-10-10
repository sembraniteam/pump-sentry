//! Security policy: pure invariant checks over a [`RugCheckReport`].
//!
//! Separated from HTTP fetching so the rules are unit-testable and
//! reusable (e.g. for cached reports). [`assess`] collects *all*
//! violations; [`RugCheckClient::evaluate_token`](crate::RugCheckClient::evaluate_token)
//! maps the first violation to [`RugCheckError::SecurityRejection`].

use crate::types::{RugCheckError, RugCheckReport};

/// Tunable acceptance thresholds. [`AuditPolicy::default`] matches the
/// skill: score cap 400, reject `danger`, reject live authorities.
#[derive(Debug, Clone)]
pub struct AuditPolicy {
    /// Reports with `score > max_score` are rejected.
    pub max_score: u32,
    /// Reject when any risk has level `danger` (case-insensitive).
    pub reject_danger: bool,
    /// Reject when a mint authority is still enabled.
    pub reject_mint_authority: bool,
    /// Reject when a freeze authority is still enabled.
    pub reject_freeze_authority: bool,
}

impl Default for AuditPolicy {
    fn default() -> Self {
        Self {
            max_score: 400,
            reject_danger: true,
            reject_mint_authority: true,
            reject_freeze_authority: true,
        }
    }
}

/// Outcome of [`assess`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Token passes every enabled invariant.
    Allow,
    /// Token fails; one entry per violated invariant.
    Reject {
        /// Human-readable violation descriptions.
        reasons: Vec<String>,
    },
}

impl Verdict {
    /// True for [`Verdict::Allow`].
    pub fn is_allow(&self) -> bool {
        matches!(self, Verdict::Allow)
    }

    /// Convert to `Result`, mapping the first reason to a rejection.
    pub fn into_result(self) -> Result<(), RugCheckError> {
        match self {
            Verdict::Allow => Ok(()),
            Verdict::Reject { reasons } => Err(RugCheckError::SecurityRejection(
                reasons
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| "rejected".to_string()),
            )),
        }
    }
}

/// Check a report against a policy, collecting every violation.
///
/// Never fails on missing data: absent `token` info simply skips the
/// authority checks.
pub fn assess(report: &RugCheckReport, policy: &AuditPolicy) -> Verdict {
    let mut reasons = Vec::new();

    if report.score > policy.max_score {
        reasons.push(format!(
            "Risk score too high: {} (max {})",
            report.score, policy.max_score
        ));
    }

    if policy.reject_danger {
        for risk in &report.risks {
            if risk.level.eq_ignore_ascii_case("danger") {
                reasons.push(format!(
                    "Critical risk detected: {} - {}",
                    risk.name, risk.description
                ));
            }
        }
    }

    if let Some(token) = &report.token {
        if policy.reject_mint_authority && token.mint_authority.is_some() {
            reasons.push("Mint authority is enabled".to_string());
        }
        if policy.reject_freeze_authority && token.freeze_authority.is_some() {
            reasons.push("Freeze authority is enabled".to_string());
        }
    }

    if reasons.is_empty() {
        Verdict::Allow
    } else {
        Verdict::Reject { reasons }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{RiskItem, TokenInfo};

    fn clean_report() -> RugCheckReport {
        RugCheckReport {
            mint: "MINT".to_string(),
            score: 100,
            risks: vec![RiskItem {
                name: "Low Liquidity".to_string(),
                value: "500".to_string(),
                description: "Thin LP".to_string(),
                score: 100,
                level: "warn".to_string(),
            }],
            token: Some(TokenInfo {
                mint_authority: None,
                freeze_authority: None,
                supply: 1_000_000,
                decimals: 6,
            }),
        }
    }

    #[test]
    fn allows_clean_report() {
        assert_eq!(
            assess(&clean_report(), &AuditPolicy::default()),
            Verdict::Allow
        );
    }

    #[test]
    fn rejects_high_score() {
        let mut report = clean_report();
        report.score = 401;
        match assess(&report, &AuditPolicy::default()) {
            Verdict::Reject { reasons } => {
                assert!(reasons.iter().any(|r| r.contains("401")));
            }
            Verdict::Allow => panic!("expected rejection"),
        }
    }

    #[test]
    fn accepts_score_at_cap() {
        let mut report = clean_report();
        report.score = 400;
        assert_eq!(assess(&report, &AuditPolicy::default()), Verdict::Allow);
    }

    #[test]
    fn rejects_danger_case_insensitive() {
        let mut report = clean_report();
        report.risks[0].level = "DANGER".to_string();
        match assess(&report, &AuditPolicy::default()) {
            Verdict::Reject { reasons } => {
                assert!(reasons.iter().any(|r| r.contains("Low Liquidity")));
            }
            Verdict::Allow => panic!("expected rejection"),
        }
    }

    #[test]
    fn rejects_live_authorities() {
        let mut report = clean_report();
        let token = report.token.as_mut().expect("token");
        token.mint_authority = Some("Auth111".to_string());
        token.freeze_authority = Some("Auth222".to_string());
        match assess(&report, &AuditPolicy::default()) {
            Verdict::Reject { reasons } => {
                assert_eq!(reasons.len(), 2);
            }
            Verdict::Allow => panic!("expected rejection"),
        }
    }

    #[test]
    fn collects_all_violations() {
        let mut report = clean_report();
        report.score = 999;
        report.risks[0].level = "danger".to_string();
        match assess(&report, &AuditPolicy::default()) {
            Verdict::Reject { reasons } => {
                assert!(reasons.len() >= 2);
            }
            Verdict::Allow => panic!("expected rejection"),
        }
    }

    #[test]
    fn skips_authority_checks_without_token_info() {
        let mut report = clean_report();
        report.token = None;
        assert_eq!(assess(&report, &AuditPolicy::default()), Verdict::Allow);
    }

    #[test]
    fn policy_toggles_disable_checks() {
        let mut report = clean_report();
        report.score = 10_000;
        report.risks[0].level = "danger".to_string();
        let token = report.token.as_mut().expect("token");
        token.mint_authority = Some("A".to_string());
        let lax = AuditPolicy {
            max_score: u32::MAX,
            reject_danger: false,
            reject_mint_authority: false,
            reject_freeze_authority: true,
        };
        assert_eq!(assess(&report, &lax), Verdict::Allow);
    }
}

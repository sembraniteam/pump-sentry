//! Security policy: pure invariant checks over a [`RugCheckReport`].
//!
//! separated from HTTP fetching so the rules are unit-testable and
//! reusable (e.g. for cached reports). [`assess`] collects *all*
//! violations; [`RugCheckClient::evaluate_token`](crate::RugCheckClient::evaluate_token)
//! joins them into the [`RugCheckError::SecurityRejection`].

use crate::types::{RugCheckError, RugCheckReport};

/// Tunable acceptance thresholds. [`AuditPolicy::default`] matches the
/// skill: score cap 400, reject `danger`, reject live authorities.
///
/// Note on the 400 vs the risk-engine heuristic's 350
/// (`risk-engine/src/fallback.rs::RUGCHECK_SCORE_CAP`): the two layers
/// answer different questions. This policy is the hard on-chain gate —
/// anything over 400 never reaches the watchlist. The 350 heuristic is
/// the softer pre-LLM triage — and the model prompt passes above 350
/// too, so scores 351–400 form a deliberate dead zone (observed, never
/// bought on any path). Fail-closed by construction.
/// Keep the strict ordering `heuristic cap <= policy cap` when tuning.
#[derive(Debug, Clone)]
pub struct AuditPolicy {
    /// Reports with `score > max_score` are rejected.
    pub max_score: u32,
    /// Reject when any risk has level `danger` (case-insensitive).
    /// Fail-closed on unknown levels: only `warn` passes, anything else
    /// (empty, `critical`, future severities) is rejected rather than
    /// waved through.
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

    /// Convert to `Result`, joining every reason into the rejection.
    pub fn into_result(self) -> Result<(), RugCheckError> {
        match self {
            Verdict::Allow => Ok(()),
            Verdict::Reject { reasons } if reasons.is_empty() => {
                Err(RugCheckError::SecurityRejection("rejected".to_string()))
            }
            Verdict::Reject { reasons } => Err(RugCheckError::SecurityRejection(
                reasons.join("; "),
            )),
        }
    }
}

/// Check a report against a policy, collecting every violation.
///
/// Fail-closed on missing data: an absent `token` section means the
/// on-chain authority checks cannot run, so the report is rejected rather
/// than waved through (a live mint authority is the classic rug).
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
            } else if !risk.level.eq_ignore_ascii_case("warn") {
                // Unknown severity (empty, `critical`, future levels):
                // fail closed instead of waving it through.
                reasons.push(format!(
                    "Unknown risk severity '{}' on {}: treating as dangerous",
                    risk.level, risk.name
                ));
            }
        }
    }

    if let Some(token) = &report.token {
        // Blank strings are as good as absent (some providers emit "").
        let mint_auth = token
            .mint_authority
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let freeze_auth = token
            .freeze_authority
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if policy.reject_mint_authority && mint_auth.is_some() {
            reasons.push("Mint authority is enabled".to_string());
        }
        if policy.reject_freeze_authority && freeze_auth.is_some() {
            reasons.push("Freeze authority is enabled".to_string());
        }
    } else {
        reasons.push("Missing on-chain token info; authorities unverifiable".to_string());
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
    fn rejects_unknown_severity() {
        // Empty (serde default), `critical`, and future levels must fail
        // closed — only `warn` passes.
        for level in ["", "critical", "HIGH", "info"] {
            let mut report = clean_report();
            report.risks[0].level = level.to_string();
            match assess(&report, &AuditPolicy::default()) {
                Verdict::Reject { reasons } => {
                    assert!(reasons.iter().any(|r| r.contains("severity")), "{level}");
                }
                Verdict::Allow => panic!("expected rejection for {level}"),
            }
        }
    }

    #[test]
    fn blank_authorities_count_as_absent() {
        let mut report = clean_report();
        let token = report.token.as_mut().expect("token");
        token.mint_authority = Some("  ".to_string());
        token.freeze_authority = Some(String::new());
        assert_eq!(assess(&report, &AuditPolicy::default()), Verdict::Allow);
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
    fn rejects_without_token_info() {
        // Fail-closed: no on-chain section means authorities cannot be
        // verified, so the report is rejected instead of waved through.
        let mut report = clean_report();
        report.token = None;
        match assess(&report, &AuditPolicy::default()) {
            Verdict::Reject { reasons } => {
                assert!(reasons.iter().any(|r| r.contains("unverifiable")));
            }
            Verdict::Allow => panic!("expected rejection"),
        }
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

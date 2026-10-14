use super::*;

/// Creation-slot bundle verdict (skill `sybil-cluster-detection`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleVerdict {
    /// No bundle evidence: organic launch.
    SafeOrganic,
    /// Bundled but below the supply monopoly threshold: suspicious,
    /// tradeable only with tightened risk.
    SuspectLowVolume,
    /// Bundled wallets control over 15% of initial supply: reject.
    RejectBundledMonopoly,
}

/// Bundle forensics over early launch swaps.
#[derive(Debug, Clone)]
pub struct BundleAnalysis {
    /// True when bundled by the creation-slot or funder rule.
    pub is_bundled: bool,
    /// Buy swaps landing in the exact creation slot.
    pub creation_slot_buys: usize,
    /// Wallets sharing a funder with at least one other wallet.
    pub shared_funder_count: usize,
    /// Combined supply share of creation-slot buys, in percent.
    pub total_cluster_supply_pct: f64,
    /// Final call.
    pub verdict: BundleVerdict,
}

/// Creation-slot bundle detector (skill `sybil-cluster-detection`).
///
/// Hard rule: a Jito bundle lands atomically in one slot, so no
/// uncoordinated outsider can transact in the token's creation slot —
/// 3+ buys there means a developer/insider bundle. Shared genesis
/// funders (2+ wallets per source) independently prove coordination.
/// Either signal sets `is_bundled`; over 15% creation-slot supply
/// escalates to [`BundleVerdict::RejectBundledMonopoly`].
///
/// Callers pass **buy** swaps only as `(slot, buyer, supply_pct)` plus
/// known `(buyer, funder)` ancestry pairs (empty when untraced — the
/// slot rule still fires).
pub struct BundleDetector;

impl BundleDetector {
    /// Evaluate early swaps against the creation slot.
    pub fn evaluate_launch_slot(
        creation_slot: u64,
        early_swaps: &[(u64, Pubkey, f64)],
        funding_parents: &[(Pubkey, Pubkey)],
    ) -> BundleAnalysis {
        // 1. Creation-slot buys.
        let creation_slot_buys = early_swaps
            .iter()
            .filter(|(slot, _, _)| *slot == creation_slot)
            .count();

        // 2. Common funder ancestry.
        let mut funder_map: HashMap<Pubkey, Vec<Pubkey>> = HashMap::new();
        for (buyer, funder) in funding_parents {
            funder_map.entry(*funder).or_default().push(*buyer);
        }
        let shared_funder_count: usize = funder_map
            .values()
            .filter(|wallets| wallets.len() >= 2)
            .map(|wallets| wallets.len())
            .sum();

        // 3. Creation-slot supply share.
        let total_cluster_supply_pct: f64 = early_swaps
            .iter()
            .filter(|(slot, _, _)| *slot == creation_slot)
            .map(|(_, _, pct)| pct)
            .sum();

        let is_bundled = creation_slot_buys >= 3 || shared_funder_count >= 2;

        let verdict = if is_bundled && total_cluster_supply_pct > 15.0 {
            BundleVerdict::RejectBundledMonopoly
        } else if is_bundled {
            BundleVerdict::SuspectLowVolume
        } else {
            BundleVerdict::SafeOrganic
        };

        BundleAnalysis {
            is_bundled,
            creation_slot_buys,
            shared_funder_count,
            total_cluster_supply_pct,
            verdict,
        }
    }
}

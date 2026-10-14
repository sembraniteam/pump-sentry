use super::*;

/// Creation-slot bundle verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleVerdict {
    /// No bundle evidence: organic launch.
    SafeOrganic,
    /// Bundled but below monopoly line: suspicious.
    SuspectLowVolume,
    /// Bundled wallets hold over 15% of supply: reject.
    RejectBundledMonopoly,
}

/// Bundle forensics over early launch swaps.
#[derive(Debug, Clone)]
pub struct BundleAnalysis {
    /// True when bundled by the creation-slot or funder rule.
    pub is_bundled: bool,
    /// Buys landing in the exact creation slot.
    pub creation_slot_buys: usize,
    /// Wallets sharing a funder with at least one other wallet.
    pub shared_funder_count: usize,
    /// Supply share of creation-slot buys, in percent.
    pub total_cluster_supply_pct: f64,
    /// Final call.
    pub verdict: BundleVerdict,
}

/// Creation-slot bundle check.
///
/// 3+ buys in the creation slot means bundled.
/// 2+ wallets per funder proves coordination too.
/// Over 15% supply rejects.
///
/// Takes buy swaps as `(slot, buyer, supply_pct)` plus `(buyer, funder)`.
pub struct BundleDetector;

impl BundleDetector {
    /// Check early swaps against the creation slot.
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

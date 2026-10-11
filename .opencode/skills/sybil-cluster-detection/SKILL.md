---
name: sybil-cluster-detection
description: On-chain heuristics to detect atomic Jito creation-slot bundles, common funder ancestry, and insider supply monopolies.
license: MIT
compatibility: opencode
metadata:
  domain: onchain-forensics
  language: rust
---

## Purpose

Provides definitive on-chain detection rules for identifying developer-coordinated bundle snipes and multi-wallet sybil
rings on Pump.fun launches.

## Verified On-Chain Forensic Heuristics

### 1. The Creation-Slot Atomic Invariant (Hard Rule)

On Solana, a Jito bundle executes atomically within a single slot (~400ms):

* The creation transaction and initial buyer transactions land in the **exact same block/slot**.
* **Invariant:** No external, uncoordinated market participant can submit or include transactions in the creation slot.
* **Rule:** If $\ge 3$ distinct buy transactions share the exact slot number of the token's `Create` transaction, the
  launch is **100% bundled by the developer/insiders**.

### 2. Common Funder Ancestry (Graph Tracing)

* Bundled wallets require SOL before they can transact.
* Developers typically fund 5–15 burner wallets within a 60-minute window prior to launch using identical or rounded SOL
  amounts (e.g., exactly 0.5 SOL or 1.0 SOL each).
* **Rule:** If $\ge 2$ early buyer wallets trace their genesis funding transaction to the same source address, cluster
  them into an insider group.

### 3. Supply Monopolization Rejection Threshold

* **Rule:** If the bundled wallets or sybil cluster collectively control $> 15.0\%$ of the initial bonding curve supply,
  trigger immediate `ACTION: PASS` (fail-closed).

## Rust Implementation Reference

```rust
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct BundleAnalysis {
    pub is_bundled: bool,
    pub creation_slot_buys: usize,
    pub shared_funder_count: usize,
    pub total_cluster_supply_pct: f64,
    pub verdict: BundleVerdict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleVerdict {
    SafeOrganic,
    SuspectLowVolume,
    RejectBundledMonopoly,
}

pub struct BundleDetector;

impl BundleDetector {
    /// Evaluates whether early transactions represent an insider Jito bundle.
    /// Slot attribution uses each buyer's EARLIEST slot and the supply share
    /// counts CREATION-SLOT buys only (later accumulation must not inflate it).
    /// Only proven `Buy` sides enter the math — unattributable records never
    /// count. Resolved-below-half funder ancestry escalates Safe → Suspect.
    pub fn evaluate_launch_slot(
        creation_slot: u64,
        early_swaps: &[(u64, Pubkey, f64)], // (slot, buyer_pubkey, supply_pct)
        funding_parents: &[(Pubkey, Pubkey)], // (buyer, funder)
    ) -> BundleAnalysis {
        // 1. Check creation-slot buys
        let creation_slot_buys = early_swaps
            .iter()
            .filter(|(slot, _, _)| *slot == creation_slot)
            .count();

        // 2. Count common funders
        let mut funder_map: HashMap<Pubkey, Vec<Pubkey>> = HashMap::new();
        for (buyer, funder) in funding_parents {
            funder_map.entry(*funder).or_default().push(*buyer);
        }
        let shared_funder_wallets: usize = funder_map
            .values()
            .filter(|wallets| wallets.len() >= 2)
            .map(|w| w.len())
            .sum();

        // 3. Compute total cluster supply
        let total_cluster_supply_pct: f64 = early_swaps
            .iter()
            .filter(|(slot, _, _)| *slot == creation_slot)
            .map(|(_, _, pct)| pct)
            .sum();

        let is_bundled = creation_slot_buys >= 3 || shared_funder_wallets >= 2;

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
            shared_funder_count: shared_funder_wallets,
            total_cluster_supply_pct,
            verdict,
        }
    }
}
```
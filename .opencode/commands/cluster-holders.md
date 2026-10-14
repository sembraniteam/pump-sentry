---
description: Perform on-chain funding graph forensics on the early buyers of a token ($1 Mint Address).
agent: sybil-forensics-auditor
subtask: true
---

Analyze early-buyer distribution and detect potential sybil rings:
**Target Mint:** $ARGUMENTS

### Execution Steps (mirror `bundle_verdict_tiered` — this bot traces early BUYERS, not top holders):

1. Read recent curve swaps (paginated, 3×25) and keep proven `Buy` sides only (unmapped layouts excluded).
2. Take the first 8 distinct buyers EARLIEST-first (records arrive newest-first — newest-first selection drops
   creation-slot buyers and undercounts toward false organic); attribute each to its EARLIEST slot and sum
   creation-slot amounts only. The monopoly denominator sums ALL creation-slot buys, not just the 8 traced.
3. Walk each buyer's genesis funding (cached, 8s budget each); cluster wallets sharing a parent funder.
4. Escalate Safe → Suspect when fewer than half the walked buyers resolve (unknown is not organic).

### Expected Output Format:

```text
=== SYBIL & CLUSTER FORENSIC REPORT ===
Target Mint:          $ARGUMENTS
Creation-Slot Buys:   <count> (≥3 proves a bundle)
Cluster Wallets:
  - <buyer_1> (<creation-pct>%)
  - <buyer_2> (<creation-pct>%)
Aggregate Cluster %:  <cluster_total_pct>%
Risk Assessment:      [CLEAN | SUSPICIOUS | SYBIL_MONOPOLY (>15%)]
Recommendation:       [SAFE_TO_TRADE | PAPER_ONLY | REJECT_TOKEN]
=======================================
```

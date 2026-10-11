---
description: Perform on-chain funding graph forensics on the top holders of a token ($1 Mint Address).
agent: sybil-forensics-auditor
subtask: true
---

Analyze holder distribution and detect potential sybil rings:
**Target Mint:** $ARGUMENTS

### Execution Steps:

1. Fetch the top 10 token holders from the mint's largest accounts.
2. For each holder, query the initial SOL funding transaction.
3. Group addresses that share the same parent funder.
4. Compute total supply held by the largest cluster.

### Expected Output Format:

```text
=== SYBIL & CLUSTER FORENSIC REPORT ===
Target Mint:          $ARGUMENTS
Top 10 Supply Share:  <pct>%
Clusters Detected:    <count>
Common Funder Wallet: <funder_pubkey_or_none>
Cluster Wallets:
  - <holder_1> (<pct_held>%)
  - <holder_2> (<pct_held>%)
Aggregate Cluster %:  <cluster_total_pct>%
Risk Assessment:      [CLEAN | SUSPICIOUS | SYBIL_MONOPOLY (>15%)]
Recommendation:       [SAFE_TO_TRADE | REJECT_TOKEN]
=======================================
```
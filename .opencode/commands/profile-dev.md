---
description: Investigate deployer wallet address for previous rugpulls and funding ancestry.
agent: deployer-wallet-profiler
subtask: true
---

Analyze the developer wallet address:
**Deployer Address:** $ARGUMENTS

### Verification Tasks:

1. Query transaction history for previous token mint deployments.
2. Calculate average survival lifespan of prior tokens.
3. Identify initial SOL funding origin (CEX, Bridge, or Sybil Transfer).

### Expected Output Format:

```text
=== DEPLOYER FORENSIC REPORT ===
Address:           $ARGUMENTS
Account Age:       <days/hours>
Mints Deployed:    <count>
Rugpull History:   <detected_count>
Funding Ancestry:  <CEX_Name | Mixer | Fresh_Wallet>
Reputation Score:  <0.00 to 1.00>
Risk Category:     [WHITELISTED | NEUTRAL | SUSPECT | BLACKLISTED]
Action Directive:  [ALLOW | BLOCK]
================================
```

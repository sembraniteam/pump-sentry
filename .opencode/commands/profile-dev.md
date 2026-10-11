---
description: Investigate deployer wallet address for previous rugpulls and funding ancestry.
agent: deployer-wallet-profiler
subtask: true
---

Analyze the developer wallet address:
**Deployer Address:** $ARGUMENTS

### Verification Tasks (mirror `screen_developer` + `judge_developer`):

1. Query signature history (capped pages): wallet age from oldest activity; empty history fails closed.
2. Sample recent transactions (bounded fanout) and count prior Pump.fun `create`/`create_v2` launches; reject serial
   launchers over the max. Majority-unreadable samples fail closed (undercounts pass sybils).
3. Identify genesis funding via system `transfer`/`createAccount`/seeded variants (incl. inner/CPI-routed) plus
   payer-paid ATA creation (best-effort; raw SPL transfers/mixer/CEX attribution is NOT implemented —
   report untraceable as such, never as clean).
4. Map survivors to the continuous trust score (0.7 at minimum age → 1.0 at 4x, −0.05 per prior, floor 0.4).

### Expected Output Format:

```text
=== DEPLOYER FORENSIC REPORT ===
Address:           $ARGUMENTS
Account Age:       <days/hours>
Mints Deployed:    <count>
Sample Trust:      [TRUSTWORTHY | UNREADABLE_FAIL_CLOSED]
Trust Score:       <0.40 to 1.00>
Risk Category:     [PASS | REJECT]
Action Directive:  [ALLOW | BLOCK]
================================
```

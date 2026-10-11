---
description:
  Query on-chain bonding curve reserves and calculate percentage towards Raydium graduation ($1: Mint Address).
agent: raydium-migration-listener
subtask: true
---

Inspect graduation progress for the following token:
**Target Mint:** $ARGUMENTS

### Execution Steps:

1. Fetch virtual and real SOL reserves from the token's bonding curve account.
2. Compute graduation percentage against the ~84 SOL threshold:
   $$\text{Progress} = \left (\frac{\text{Real SOL Lamports}}{84,000,000,000}\right) \times 100$$
3. Display current market cap estimate and distance to migration.

### Expected Output Format:

```text
=== PUMP.FUN GRADUATION MIGRATION STATUS ===
Token Mint:           $ARGUMENTS
Real SOL in Curve:    <amount> SOL / 84.00 SOL
Curve Completion:     <pct>%
Market Cap Estimate:  $<usd_mcap>
Status:               [BONDING | NEAR_GRADUATION (>=90%) | GRADUATED_TO_RAYDIUM]
Action Directive:     [MONITOR | PREPARE_RAYDIUM_SNIPE | TRADING_ON_RAYDIUM]
============================================
```
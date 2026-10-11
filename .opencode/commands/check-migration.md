---
description:
  Query on-chain bonding curve reserves and calculate percentage towards Raydium graduation ($1: Mint Address).
agent: raydium-migration-listener
subtask: true
---

Inspect graduation progress for the following token:
**Target Mint:** $ARGUMENTS

### Execution Steps:

1. Read the bonding-curve account: verify the discriminator first (byte 48 on a foreign account is NOT graduation),
   then virtual/real reserves and the `complete` flag.
2. Compute graduation percentage against the ~84 SOL threshold:
   $$\text{Progress} = \left (\frac{\text{Real SOL Lamports}}{84,000,000,000}\right) \times 100$$
3. If graduated, classify the migration tx (Raydium needs program + `initialize2` log + watched-curve/base linkage;
   PumpSwap needs `migrate` discriminator or `MigrateV2` log + linkage) and resolve the pool through the trust gate
   (Raydium-owned, WSOL quote, expected base). Unresolvable pools report pool-less, loudly — never silently.
4. Display current market cap estimate and distance to migration. Paper tracking only — no live pool entries exist.

### Expected Output Format:

```text
=== PUMP.FUN GRADUATION MIGRATION STATUS ===
Token Mint:           $ARGUMENTS
Real SOL in Curve:    <amount> SOL / 84.00 SOL
Curve Completion:     <pct>%
Market Cap Estimate:  $<usd_mcap>
Migration:            [NONE | RAYDIUM <amm> | PUMPSWAP | UNRESOLVED_POOL]
Status:               [BONDING | NEAR_GRADUATION (>=90%) | GRADUATED_TO_RAYDIUM]
Action Directive:     [MONITOR | TRACK_POOL_PAPER | TRADING_ON_RAYDIUM]
============================================
```

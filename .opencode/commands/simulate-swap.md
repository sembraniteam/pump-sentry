---
description: Simulate a paper trade entry and verify execution metrics ($1 Mint, $2 SOL_Amount).
agent: paper-trading-simulator
subtask: true
---

Simulate a paper trade execution for:

- **Token Mint:** $1
- **Trade Size (SOL):** $2

### Verification Tasks:

1. Calculate expected token output based on bonding curve reserves.
2. Deduct simulated priority fees (0.001 SOL) and DEX swap fees (0.25%).
3. Register the open position into the virtual portfolio ledger.

### Expected Output Format:

```text
=== SIMULATED EXECUTION TICKET ===
Timestamp:        <UTC_ISO_8601>
Token Mint:       $1
Allocated:        $2 SOL
Fill Price (SOL): <simulated_fill_price>
Price Impact:     <calculated_pct>%
Estimated Fees:   <total_fees_in_sol> SOL
Position ID:      <uuid>
Portfolio Cash:   <remaining_virtual_cash> SOL
Active Positions: <count>
==================================
```

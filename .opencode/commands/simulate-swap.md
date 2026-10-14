---
description: Simulate a paper trade entry and verify execution metrics ($1 Mint, $2 USD_Notional).
agent: paper-trading-simulator
subtask: true
---

Simulate a paper trade execution for:

- **Token Mint:** $1
- **USD Notional:** $2 (paper sizes in USD at curve spot — never SOL amounts)

### Verification Tasks (mirror `crates/simulator`):

1. Price the fill through `FillModel::BondingCurve` (size impact + 125 bps/side venue fee, 25% cap) — never at spot.
2. Charge `fee_bps` commission on notionals on top (default 0 = no commission; never 125 on the default model or the venue
   fee double-counts).
3. Open via `open_position_params` (explicit rejects: empty mint, bad price/notional/TP/SL/fraction) and report the
   numeric position id. Exits: TP/SL/trailing/timeout; TakeProfit scale-outs ratchet to move-breakeven, lift the
   time-stop, and disarm the moonbag — Manual trims preserve terms.

### Expected Output Format:

```text
=== SIMULATED EXECUTION TICKET ===
Timestamp:        <unix_secs>
Token Mint:       $1
Notional:         $2 USD
Fill Price:       <simulated_fill_price_usd>
Price Impact:     <calculated_pct>%
Entry Fee:        <commission_usd> USD
Position ID:      <u64>
Portfolio Cash:   <remaining_virtual_cash> USD
Active Positions: <count>
==================================
```

---
description: Emergency kill-switch market dump all held Pump.fun tokens back to SOL and halt the trading pipeline.
agent: solana-swap-executor
subtask: true
---

Trigger emergency shutdown and market liquidation:

### Immediate Actions:

1. Query all non-zero SPL token accounts owned by the wallet.
2. For each token held on Pump.fun bonding curves, build the exit via `build_sell_v3_ix`
   (`SellV3Accounts`, v3 17-account — NEVER a retired 12/18/27/26-account builder):
    - Sell 100% of the tracked balance (floor, never ceiling — never oversell custody).
    - Sell 100% of the tracked balance (floor, never ceiling — never oversell custody).
    - Set an explicit `min_sol_output` floor (preflight rejects 0 — a zero floor authorizes any fill price).
    - Bundle with a Jito tip for priority inclusion; preflight `simulateTransaction` first.
    - Sign and dispatch; unconfirmed sells stay tracked with cooldown + 5-attempt budget, then escalate to manual
      exit — never assume unfilled.
3. Emit shutdown alert and display realized SOL recovered.

### Output Contract:

```text
=== EMERGENCY LIQUIDATION SUMMARY ===
Tokens Liquidated: <count>
Total SOL Recovered: <recovered_sol> SOL
Unconfirmed (manual review): <count>
Pipeline Status: [HALTED]
Signatures:
  - <token_mint>: https://explorer.solana.com/tx/<signature>
====================================
```

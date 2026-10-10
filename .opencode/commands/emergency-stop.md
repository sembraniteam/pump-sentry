---
description: Emergency kill-switch market dump all held Pump.fun tokens back to SOL and halt the trading pipeline.
agent: solana-swap-executor
subtask: true
---

Trigger emergency shutdown and market liquidation:

### Immediate Actions:

1. Query all non-zero SPL token accounts owned by the wallet.
2. For each token held on Pump.fun bonding curves:
    - Construct `PumpSwapBuilder::build_sell_ix` for 100% of token balance.
    - Set maximum slippage (1000 bps / 10%) to guarantee immediate exit.
    - Bundle with aggressive Jito tip (0.005 SOL) for instant priority inclusion.
    - Sign and dispatch transaction.
3. Emit shutdown alert and display realized SOL recovered.

### Output Contract:

```text
=== EMERGENCY LIQUIDATION SUMMARY ===
Tokens Liquidated: <count>
Total SOL Recovered: <recovered_sol> SOL
Pipeline Status: [HALTED]
Signatures:
  - <token_mint>: https://solscan.io/tx/<signature>
=====================================
```
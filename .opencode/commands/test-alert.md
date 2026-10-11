---
description: Dispatch a mock trade alert to Discord via Hermes CLI to verify notification delivery.
agent: solana-swap-executor
subtask: true
---

Trigger a test notification into Discord using `hermes send`:

### Execution:

1. Construct sample payload:
    - Target Mint: `DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263`
    - Event: `TEST_TRADE_ALERT`
    - Realized PnL: `+$3.85 (+38.5%)`
2. Run `hermes send --to discord` with formatted markdown.
3. Confirm delivery returncode.

### Output:

```text
=== DISCORD NOTIFICATION TEST ===
Status: [DISPATCHED]
Destination: Discord default channel
Message Payload:
  🟢 [TEST ALERT] Solana Memecoin Sniper alert delivery verified!
=================================
```
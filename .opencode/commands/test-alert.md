---
description: Dispatch a mock trade alert to Discord via Hermes CLI to verify notification delivery.
agent: solana-swap-executor
subtask: true
---

Trigger a test notification into Discord using `hermes send` (binary from `HERMES_BIN` env, else `hermes` on PATH —
prefer absolute):

### Execution:

1. Construct sample payload:
    - Target Mint: `DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263`
    - Event: `TEST_TRADE_ALERT`
    - Realized PnL: `+$3.85 (+38.5%)`
2. Run `hermes send --to discord` with a formatter-built markdown card (positional message; at most 4 concurrent
   children — excess drops with a counter, never fork-bombs).
3. Confirm delivery returncode (5s timeout kills the child; failures warn-only with capped stderr).

### Output:

```text
=== DISCORD NOTIFICATION TEST ===
Status: [DISPATCHED | DROPPED_STORM | FAILED_WARN_ONLY]
Destination: Discord default channel
Message Payload:
  🟢 [TEST ALERT] Solana Memecoin Sniper alert delivery verified!
=================================
```

---
description: Compute dynamic TP/SL levels ($1 Liquidity, $2 BuyVelocity, $3 DevScore).
agent: ai-risk-engine
subtask: true
---

Calculate dynamic risk targets using:

- **Pool Liquidity (USD):** $1
- **5-Second Buy Velocity (Txs):** $2
- **Deployer Reputation Score (0.4-1.0):** $3

### Processing Rules (mirror `crates/risk-engine` — no invented fields):

1. Send prompt to configured LLM inference provider (OpenAI-compatible). Budget default 300ms (hard ceiling 2s).
2. Enforce admission bounds: BUY needs TP in (0, 1000%], SL in [-100%, 0), timeout ≤ 3600s.
3. If buy velocity < 5 or dev < 0.40 or score > 350 or liquidity < $10k, action MUST be PASS.
4. If inference exceeds budget, errors, or returns invalid JSON, use the deterministic fallback (high: 60/-10/180s/
   moonbag 20; moderate: 35/-8/90s; else PASS).
5. Strategy shaping happens downstream (`adaptive` verbatim; `scalp` clamps TP down to +35%; `runner` forces moonbag
    + trailing) — report the RAW profile here.

### Expected Output Format:

```json
{
  "action": "BUY",
  "tp_percent": 45.0,
  "sl_percent": -8.0,
  "trailing_stop": true,
  "timeout_seconds": 120,
  "moonbag_percent": 20.0,
  "rationale": "High buy velocity with clean dev history."
}
```

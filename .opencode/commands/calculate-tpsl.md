---
description: Compute dynamic TP/SL levels ($1 Liquidity, $2 BuyVelocity, $3 DevScore).
agent: ai-risk-engine
subtask: true
---

Calculate dynamic risk targets using:

- **Initial Pool Liquidity (USD):** $1
- **5-Second Buy Velocity (Txs):** $2
- **Deployer Reputation Score (0.0-1.0):** $3

### Processing Rules:

1. Send prompt to configured LLM inference provider (OpenAI-compatible / Hermes Agent).
2. Enforce limits: Max SL = -15%, Min SL = -8.0% (to absorb AMM spread), Min TP = +30.0%, Max Timeout = 300s.
3. If buy velocity < 5 or liquidity < $10k, action MUST be PASS.
4. If inference > 1500ms, use deterministic fallback.

### Expected Output Format:

```json
{
  "action": "BUY",
  "tp_percent": 45.0,
  "sl_percent": -8.0,
  "trailing_stop": true,
  "timeout_seconds": 120,
  "confidence_score": 0.88,
  "rationale": "High buy velocity with clean dev history."
}
```
---
name: adaptive-risk-prompting
description: Model-agnostic prompt schemas, JSON response contracts, and deterministic fallback rules for dynamic TP/SL.
license: MIT
compatibility: opencode
metadata:
  domain: quantitative-risk
---

## Purpose

Defines the input formatting, temperature constraints, and fallback logic for LLM-driven Take-Profit / Stop-Loss
calculations.

## Prompt Specification

```text
SYSTEM:
You are an ultra-fast quantitative risk engine for volatile Solana memecoins.
Evaluate token metrics and return optimal Take-Profit (TP) and Stop-Loss (SL) percentages.
Output strictly valid JSON with no markdown formatting, preamble, or comments.

INPUT PAYLOAD:
{
  "liquidity_usd": <number>,
  "buy_velocity_5s": <number>,
  "dev_reputation": <0.0-1.0>,
  "rugcheck_score": <number>
}

OUTPUT SCHEMA:
{
  "action": "BUY" | "PASS",
  "tp_percent": <float>,
  "sl_percent": <float>,
  "trailing_stop": <bool>,
  "timeout_seconds": <int>
}

RULES:
- If dev_reputation < 0.35 OR rugcheck_score > 350 -> "action": "PASS".
- High Momentum (velocity > 30, liq > $25k) -> TP: 60.0 to 120.0, SL: -8.0, trailing: true, timeout: 180.
- Moderate Momentum (velocity 10-30, liq $10k-$25k) -> TP: 25.0 to 45.0, SL: -6.0, trailing: false, timeout: 90.
- Low Liquidity Scalp (liq < $10k) -> TP: 15.0 to 25.0, SL: -4.0, trailing: false, timeout: 45.
```

## Deterministic Heuristic Fallback (Rust)

Executes instantly if HTTP inference times out (>300ms):

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RiskProfile {
    pub action: String,
    pub tp_percent: f64,
    pub sl_percent: f64,
    pub trailing_stop: bool,
    pub timeout_seconds: u64,
}

pub fn evaluate_heuristic_fallback(
    liq_usd: f64,
    buy_velocity_5s: u32,
    dev_rep: f64,
    rugcheck_score: u32,
) -> RiskProfile {
    if dev_rep < 0.35 || rugcheck_score > 350 {
        return RiskProfile {
            action: "PASS".into(),
            tp_percent: 0.0,
            sl_percent: 0.0,
            trailing_stop: false,
            timeout_seconds: 0,
        };
    }

    if buy_velocity_5s > 30 && liq_usd > 25_000.0 {
        RiskProfile {
            action: "BUY".into(),
            tp_percent: 75.0,
            sl_percent: -8.0,
            trailing_stop: true,
            timeout_seconds: 180,
        }
    } else if buy_velocity_5s >= 10 {
        RiskProfile {
            action: "BUY".into(),
            tp_percent: 30.0,
            sl_percent: -6.0,
            trailing_stop: false,
            timeout_seconds: 90,
        }
    } else {
        RiskProfile {
            action: "BUY".into(),
            tp_percent: 18.0,
            sl_percent: -4.0,
            trailing_stop: false,
            timeout_seconds: 45,
        }
    }
}
```
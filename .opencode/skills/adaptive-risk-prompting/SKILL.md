---
name: adaptive-risk-prompting
description: Model-agnostic prompt schemas, JSON response contracts, and deterministic fallback rules for dynamic TP/SL.
license: MIT
compatibility: opencode
metadata:
  domain: quantitative-risk
  protocol: openai-compatible-json
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
  "timeout_seconds": <int>,
  "moonbag_percent": <float>
}

RULES:
- If dev_reputation < 0.40 OR rugcheck_score > 350 -> "action": "PASS".
- If buy_velocity_5s < 5 (no confirmed momentum/dead launch) -> "action": "PASS".
- High Momentum (velocity > 25, liq > $20k) -> TP: 45.0 to 100.0, SL: -10.0, trailing: true, timeout: 180, moonbag: 20.0.
- Moderate Momentum (velocity 5-25, liq >= $10k) -> TP: 30.0 to 50.0, SL: -8.0, trailing: false, timeout: 90, moonbag: 0.0.
- Otherwise (low liquidity or stagnant volume) -> "action": "PASS" (Never buy dead tokens).

ADMISSION BOUNDS (model output must satisfy `is_valid`, else fallback):
- BUY: TP in (0, 1000%], SL in [-100%, 0), timeout <= 3600s. Unbounded magnitudes would never TP/expire.
- Strategy shaping downstream: `adaptive` verbatim; `scalp` clamps TP DOWN to +35% (below-band honored, never
  widened); `runner` forces moonbag + trailing. Invalid profiles pass through untouched for the caller to reject.
- Inference budget default 300ms (hard ceiling 2s at build); endpoint must parse as http/https.
```

## Deterministic Heuristic Fallback (Rust)

Executes instantly if HTTP inference times out. Uses the `Action` enum (`BUY` | `PASS`), never bare strings:

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Action {
    #[serde(rename = "BUY")]
    Buy,
    #[serde(rename = "PASS")]
    Pass,
}

pub struct RiskProfile {
    pub action: Action,
    pub tp_percent: f64,
    pub sl_percent: f64,
    pub trailing_stop: bool,
    pub timeout_seconds: u64,
    pub moonbag_percent: f64,
}

pub fn evaluate_heuristic_fallback(
    liq_usd: f64,
    buy_velocity_5s: u32,
    dev_rep: f64,
    rugcheck_score: u32,
) -> RiskProfile {
    // 1. Hard safety and minimum momentum filter
    if dev_rep < 0.40 || rugcheck_score > 350 || buy_velocity_5s < 5 {
        return RiskProfile {
            action: Action::Pass,
            tp_percent: 0.0,
            sl_percent: 0.0,
            trailing_stop: false,
            timeout_seconds: 0,
            moonbag_percent: 0.0,
        };
    }

    // 2. High momentum breakout
    if buy_velocity_5s > 25 && liq_usd > 20_000.0 {
        RiskProfile {
            action: Action::Buy,
            tp_percent: 60.0,
            sl_percent: -10.0,
            trailing_stop: true,
            timeout_seconds: 180,
            moonbag_percent: 20.0,
        }
    // 3. Moderate confirmed volume
    } else if buy_velocity_5s >= 5 && liq_usd >= 10_000.0 {
        RiskProfile {
            action: Action::Buy,
            tp_percent: 35.0,
            sl_percent: -8.0,
            trailing_stop: false,
            timeout_seconds: 90,
            moonbag_percent: 0.0,
        }
    // 4. Default: Reject unproven liquidity/velocity
    } else {
        RiskProfile {
            action: Action::Pass,
            tp_percent: 0.0,
            sl_percent: 0.0,
            trailing_stop: false,
            timeout_seconds: 0,
            moonbag_percent: 0.0,
        }
    }
}
```

> Pre-step 0 in the repo (not shown above): fail closed on any non-finite input (`!liq_usd.is_finite() ||
> !dev_rep.is_finite()` → PASS), because NaN comparisons are all false and would otherwise sail straight into the
> momentum branches. All PASS shapes share one constructor so the branches cannot drift.

# risk-engine

LLM-driven Take-Profit / Stop-Loss engine with a deterministic fallback.
Consumes the prompt schema from skill `adaptive-risk-prompting`.

## Architecture

```
evaluate(metrics)
  ├─ POST {endpoint} (OpenAI-compatible chat, temperature 0.1, JSON mode)
  │    └─ valid in-schema profile within 300ms → DecisionSource::Model
  └─ timeout / error / non-200 / bad JSON / invalid profile
       └─ evaluate_heuristic_fallback → DecisionSource::Fallback
```

- `schema` — `TokenMetrics` → skill `INPUT PAYLOAD`; `RiskProfile`
  (`BUY|PASS`, `tp_percent`, `sl_percent`, `trailing_stop`,
  `timeout_seconds`) + `is_valid()` guard.
- `fallback` — skill decision table hardened by live results (0% win
  rate on no-demand buys): PASS when `dev_reputation < 0.35`,
  `rugcheck_score > 350`, or `velocity < 5`; high momentum (vel > 30, liq > $25k) → TP 75 / SL −8 / trail / 180s;
  moderate (vel ≥ 10) → TP 30 / SL −8 / 90s; thin demand (vel 5–9) → TP 40 /
  SL −10 / 45s. Bonding-curve spread (~2–4%) makes tighter stops
  instant losses.
- `engine` — `RiskEngine` (pooled HTTP client, `FALLBACK_TIMEOUT` =
  300ms). Model output is schema-validated; anything unusable resolves
  to the heuristic, so `evaluate` never fails on model trouble.

## Use

```rust
use risk_engine::{RiskEngine, TokenMetrics};

let engine = RiskEngine::builder()
.endpoint("http://127.0.0.1:11434/v1/chat/completions")
.model("llama3.2:1b")
.build() ?;

let decision = engine.evaluate( & TokenMetrics {
liquidity_usd: 30_000.0,
buy_velocity_5s: 40,
dev_reputation: 0.8,
rugcheck_score: 120,
}).await;

match decision.profile.action {
risk_engine::Action::Buy => println!("BUY tp={} sl={}", decision.profile.tp_percent, decision.profile.sl_percent),
risk_engine::Action::Pass => println!("PASS"),
}
```

## Test

```sh
cargo test -p risk-engine
```
